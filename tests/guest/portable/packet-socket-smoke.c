#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <linux/if_ether.h>
#include <linux/filter.h>
#include <linux/if_packet.h>
#include <limits.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <poll.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "packet smoke test requires the x86_64 Linux ABI"
#endif
#ifndef SYS_capset
#define SYS_capset 126
#endif

#ifndef PACKET_IGNORE_OUTGOING
#define PACKET_IGNORE_OUTGOING 23
#endif

#ifndef SO_PROTOCOL
#define SO_PROTOCOL 38
#endif

#ifndef SO_DOMAIN
#define SO_DOMAIN 39
#endif

#ifndef SKF_AD_OFF
#define SKF_AD_OFF (-0x1000)
#endif

#ifndef SKF_AD_PROTOCOL
#define SKF_AD_PROTOCOL 0
#endif

#ifndef SKF_AD_IFINDEX
#define SKF_AD_IFINDEX 8
#endif

#define CUSTOM_PROTOCOL 0x88b5U
#define WAIT_MILLISECONDS 1500
#define QUIET_MILLISECONDS 80
#define MAX_RECORD_BYTES 4096
#define MAX_MATCHING_RECORDS 4
#define BPF_EXECUTOR_CONTROL_PATH "/proc/bpf_executor_control"
#define BPF_STATS_PATH "/proc/bpf_stats"
#define BPF_STATS_BUFFER 4096U

struct packet_record {
    unsigned char data[MAX_RECORD_BYTES];
    ssize_t length;
    struct sockaddr_ll address;
    socklen_t address_length;
    int message_flags;
};

struct udp_pair {
    int sender;
    int receiver;
    struct sockaddr_in destination;
};

struct extended_sockaddr_ll {
    struct sockaddr_ll address;
    unsigned char ninth_address_byte;
};

_Static_assert(offsetof(struct extended_sockaddr_ll, ninth_address_byte) ==
                   sizeof(struct sockaddr_ll),
               "sockaddr_ll extension must immediately follow sll_addr");

static unsigned int loopback_index;
static unsigned int token_counter;
static bool linux_host_mode;
static bool require_options;

enum cbpf_executor_mode {
    CBPF_EXECUTOR_AUTO,
    CBPF_EXECUTOR_INTERPRETER,
    CBPF_EXECUTOR_JIT,
};

struct cbpf_counters {
    unsigned long long published;
    unsigned long long native_executed;
    unsigned long long interpreter_executed;
    unsigned long long fallback_policy_interpreter;
    unsigned long long fallback_translation;
    unsigned long long fallback_publication;
    unsigned long long fallback_owner;
    unsigned long long fallback_unavailable;
    unsigned long long jit_rejected;
};

static void fail_message(const char *stage, const char *detail) {
    fprintf(stderr, "THEKERNEL_PACKET_FAIL %s detail=%s errno=%d (%s)\n",
            stage, detail, errno, strerror(errno));
    exit(EXIT_FAILURE);
}

static void fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr,
            "THEKERNEL_PACKET_FAIL %s actual=%ld expected=%ld errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    exit(EXIT_FAILURE);
}

static void require_true(bool condition, const char *stage) {
    if (!condition) {
        fail_message(stage, "condition-false");
    }
}

static void marker(const char *value) {
    puts(value);
    fflush(stdout);
}

static int64_t monotonic_milliseconds(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
        fail_message("clock-gettime", "failed");
    }
    return (int64_t)value.tv_sec * 1000 + value.tv_nsec / 1000000;
}

static bool contains_bytes(const unsigned char *haystack, size_t haystack_len,
                           const unsigned char *needle, size_t needle_len) {
    if (needle_len == 0 || haystack_len < needle_len) {
        return false;
    }
    for (size_t offset = 0; offset <= haystack_len - needle_len; ++offset) {
        if (memcmp(haystack + offset, needle, needle_len) == 0) {
            return true;
        }
    }
    return false;
}

static void make_token(unsigned char output[16], unsigned char group,
                       unsigned char test_case) {
    static const unsigned char prefix[8] = {'T', 'K', 'P', 'K', 'T', '0', '1', '!'};
    unsigned int serial = ++token_counter;
    memcpy(output, prefix, sizeof(prefix));
    output[8] = group;
    output[9] = test_case;
    output[10] = (unsigned char)(serial >> 24);
    output[11] = (unsigned char)(serial >> 16);
    output[12] = (unsigned char)(serial >> 8);
    output[13] = (unsigned char)serial;
    output[14] = 0xa5;
    output[15] = 0x5a;
}

static int packet_socket(int type, unsigned int protocol) {
    int fd = socket(AF_PACKET, type | SOCK_NONBLOCK, htons((uint16_t)protocol));
    if (fd < 0) {
        fail_message("socket-packet", "create");
    }
    int flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0 || !(flags & O_NONBLOCK)) {
        fail_message("socket-packet", "nonblocking-flag");
    }
    return fd;
}

static void bind_packet(int fd, unsigned int protocol, unsigned int ifindex) {
    struct sockaddr_ll address;
    memset(&address, 0, sizeof(address));
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons((uint16_t)protocol);
    address.sll_ifindex = (int)ifindex;
    if (bind(fd, (const struct sockaddr *)&address, sizeof(address)) != 0) {
        fail_message("bind-packet", "bind");
    }
}

static int bound_packet_socket(int type, unsigned int protocol,
                               unsigned int ifindex) {
    int fd = packet_socket(type, protocol);
    bind_packet(fd, protocol, ifindex);
    return fd;
}

static void close_checked(int fd, const char *stage) {
    if (close(fd) != 0) {
        fail_message(stage, "close");
    }
}

static int read_proc_text(const char *path, char *buffer, size_t capacity) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        if (errno == ENOENT || errno == ENOTDIR) {
            return 0;
        }
        return -1;
    }
    size_t length = 0;
    for (;;) {
        if (length + 1U >= capacity) {
            errno = EOVERFLOW;
            close(fd);
            return -1;
        }
        ssize_t count = read(fd, buffer + length, capacity - length - 1U);
        if (count == 0) {
            break;
        }
        if (count < 0) {
            if (errno == EINTR) {
                continue;
            }
            int saved_errno = errno;
            close(fd);
            errno = saved_errno;
            return -1;
        }
        length += (size_t)count;
    }
    if (close(fd) != 0) {
        return -1;
    }
    buffer[length] = '\0';
    return 1;
}

static int write_all(int fd, const char *data, size_t length) {
    size_t offset = 0;
    while (offset < length) {
        ssize_t count = write(fd, data + offset, length - offset);
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count <= 0) {
            return -1;
        }
        offset += (size_t)count;
    }
    return 0;
}

static const char *cbpf_executor_name(enum cbpf_executor_mode mode) {
    switch (mode) {
    case CBPF_EXECUTOR_AUTO:
        return "auto";
    case CBPF_EXECUTOR_INTERPRETER:
        return "interpreter";
    case CBPF_EXECUTOR_JIT:
        return "jit";
    }
    return "unknown";
}

static int set_cbpf_executor(enum cbpf_executor_mode mode) {
    int fd = open(BPF_EXECUTOR_CONTROL_PATH, O_WRONLY);
    if (fd < 0) {
        if (errno == ENOENT || errno == ENOTDIR) {
            return 0;
        }
        return -1;
    }
    char request[64];
    int length = snprintf(request, sizeof(request), "packet=%s\n",
                          cbpf_executor_name(mode));
    if (length <= 0 || (size_t)length >= sizeof(request) ||
        write_all(fd, request, (size_t)length) != 0) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return -1;
    }
    if (close(fd) != 0) {
        return -1;
    }

    char control[BPF_STATS_BUFFER];
    int result = read_proc_text(BPF_EXECUTOR_CONTROL_PATH, control,
                                sizeof(control));
    if (result != 1) {
        errno = result == 0 ? ENOENT : errno;
        return -1;
    }
    bool found = false;
    char *save = NULL;
    for (char *line = strtok_r(control, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        if (strncmp(line, "packet=", 7) == 0) {
            found = strcmp(line + 7, cbpf_executor_name(mode)) == 0;
            break;
        }
    }
    if (!found) {
        errno = EPROTO;
        return -1;
    }
    return 1;
}

static int parse_counter(const char *text, unsigned long long *value) {
    char *end = NULL;
    errno = 0;
    unsigned long long parsed = strtoull(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0') {
        return -1;
    }
    *value = parsed;
    return 0;
}

static int read_cbpf_stats(struct cbpf_counters *counters) {
    char buffer[BPF_STATS_BUFFER];
    int result = read_proc_text(BPF_STATS_PATH, buffer, sizeof(buffer));
    if (result != 1) {
        return result;
    }
    memset(counters, 0, sizeof(*counters));
    bool seen[9] = {false};
    char *save = NULL;
    for (char *line = strtok_r(buffer, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        char key[64];
        char value[64];
        char extra[2];
        if (sscanf(line, "%63s %63s %1s", key, value, extra) != 2 ||
            strncmp(key, "packet.", 7) != 0) {
            continue;
        }
        const char *field = key + 7;
        unsigned long long *destination = NULL;
        unsigned int index = 0;
        if (strcmp(field, "published") == 0) {
            destination = &counters->published;
            index = 0;
        } else if (strcmp(field, "native_executed") == 0) {
            destination = &counters->native_executed;
            index = 1;
        } else if (strcmp(field, "interpreter_executed") == 0) {
            destination = &counters->interpreter_executed;
            index = 2;
        } else if (strcmp(field, "fallback.policy_interpreter") == 0) {
            destination = &counters->fallback_policy_interpreter;
            index = 3;
        } else if (strcmp(field, "fallback.translation") == 0) {
            destination = &counters->fallback_translation;
            index = 4;
        } else if (strcmp(field, "fallback.publication") == 0) {
            destination = &counters->fallback_publication;
            index = 5;
        } else if (strcmp(field, "fallback.owner") == 0) {
            destination = &counters->fallback_owner;
            index = 6;
        } else if (strcmp(field, "fallback.unavailable") == 0) {
            destination = &counters->fallback_unavailable;
            index = 7;
        } else if (strcmp(field, "jit_rejected") == 0) {
            destination = &counters->jit_rejected;
            index = 8;
        }
        if (destination == NULL || seen[index] ||
            parse_counter(value, destination) != 0) {
            errno = EPROTO;
            return -1;
        }
        seen[index] = true;
    }
    for (unsigned int index = 0; index < 9U; ++index) {
        if (!seen[index]) {
            errno = EPROTO;
            return -1;
        }
    }
    return 1;
}

static bool subtract_counter(unsigned long long after,
                             unsigned long long before,
                             unsigned long long *delta) {
    if (after < before) {
        return false;
    }
    *delta = after - before;
    return true;
}

static void udp_pair_open(struct udp_pair *pair) {
    memset(pair, 0, sizeof(*pair));
    pair->receiver = socket(AF_INET, SOCK_DGRAM, 0);
    pair->sender = socket(AF_INET, SOCK_DGRAM, 0);
    if (pair->receiver < 0 || pair->sender < 0) {
        fail_message("udp-pair", "socket");
    }
    pair->destination.sin_family = AF_INET;
    pair->destination.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(pair->receiver, (const struct sockaddr *)&pair->destination,
             sizeof(pair->destination)) != 0) {
        fail_message("udp-pair", "bind");
    }
    socklen_t length = sizeof(pair->destination);
    if (getsockname(pair->receiver, (struct sockaddr *)&pair->destination,
                    &length) != 0 ||
        length != sizeof(pair->destination)) {
        fail_message("udp-pair", "getsockname");
    }
}

static void udp_pair_close(struct udp_pair *pair) {
    close_checked(pair->sender, "udp-pair-sender");
    close_checked(pair->receiver, "udp-pair-receiver");
}

static void udp_send_token(struct udp_pair *pair,
                           const unsigned char token[16]) {
    ssize_t sent = sendto(pair->sender, token, 16, 0,
                          (const struct sockaddr *)&pair->destination,
                          sizeof(pair->destination));
    if (sent != 16) {
        fail_value("udp-send", sent, 16);
    }
}

static void require_udp_token(const unsigned char copy[16], ssize_t received,
                              const unsigned char token[16],
                              const char *stage) {
    if (received != 16 || memcmp(copy, token, 16) != 0) {
        fail_value(stage, received, 16);
    }
}

static void udp_send_and_drain(struct udp_pair *pair,
                               const unsigned char token[16]) {
    udp_send_token(pair, token);
    unsigned char copy[16];
    ssize_t received = recv(pair->receiver, copy, sizeof(copy), 0);
    require_udp_token(copy, received, token, "udp-receive");
}

static void test_udp_receive_without_source_output(void) {
    struct udp_pair udp;
    udp_pair_open(&udp);
    unsigned char token[16];
    unsigned char copy[16];

    make_token(token, 0, 1);
    udp_send_token(&udp, token);
    require_udp_token(copy, recv(udp.receiver, copy, sizeof(copy), 0), token,
                      "udp-recv-null-source");

    make_token(token, 0, 2);
    udp_send_token(&udp, token);
    require_udp_token(copy,
                      recvfrom(udp.receiver, copy, sizeof(copy), 0, NULL, NULL),
                      token, "udp-recvfrom-null-source");

    make_token(token, 0, 3);
    udp_send_token(&udp, token);
    struct iovec iov = {.iov_base = copy, .iov_len = sizeof(copy)};
    struct msghdr message = {
        .msg_name = NULL,
        .msg_namelen = 0,
        .msg_iov = &iov,
        .msg_iovlen = 1,
    };
    require_udp_token(copy, recvmsg(udp.receiver, &message, 0), token,
                      "udp-recvmsg-null-source");

    udp_pair_close(&udp);
    marker("THEKERNEL_PACKET_UDP_PRECONDITION_OK");
}

static short poll_once(int fd, short events, int timeout_milliseconds) {
    struct pollfd descriptor = {.fd = fd, .events = events};
    int result = poll(&descriptor, 1, timeout_milliseconds);
    if (result < 0) {
        fail_message("packet-poll", "poll");
    }
    return result == 0 ? 0 : descriptor.revents;
}

static void receive_record(int fd, struct packet_record *record, int flags) {
    memset(record, 0, sizeof(*record));
    struct iovec iov = {.iov_base = record->data, .iov_len = sizeof(record->data)};
    struct msghdr message = {
        .msg_name = &record->address,
        .msg_namelen = sizeof(record->address),
        .msg_iov = &iov,
        .msg_iovlen = 1,
    };
    record->length = recvmsg(fd, &message, flags | MSG_DONTWAIT);
    if (record->length < 0) {
        fail_message("packet-receive", "recvmsg");
    }
    record->address_length = message.msg_namelen;
    record->message_flags = message.msg_flags;
}

static size_t collect_records(int fd, const unsigned char *needle,
                              size_t needle_len, struct packet_record *records,
                              size_t wanted) {
    require_true(wanted <= MAX_MATCHING_RECORDS, "collect-records-capacity");
    size_t count = 0;
    int64_t deadline = monotonic_milliseconds() + WAIT_MILLISECONDS;
    while (count < wanted) {
        int64_t remaining = deadline - monotonic_milliseconds();
        if (remaining <= 0) {
            fail_value("collect-records-timeout", (long)count, (long)wanted);
        }
        short events = poll_once(fd, POLLIN, (int)remaining);
        if (!(events & POLLIN)) {
            fail_value("collect-records-poll", (long)count, (long)wanted);
        }
        struct packet_record candidate;
        receive_record(fd, &candidate, 0);
        if ((size_t)candidate.length >= needle_len &&
            contains_bytes(candidate.data, (size_t)candidate.length, needle,
                           needle_len)) {
            records[count++] = candidate;
        }
    }
    return count;
}

static void require_no_matching_record(int fd, const unsigned char *needle,
                                       size_t needle_len) {
    int64_t deadline = monotonic_milliseconds() + QUIET_MILLISECONDS;
    for (;;) {
        int64_t remaining = deadline - monotonic_milliseconds();
        if (remaining <= 0) {
            return;
        }
        short events = poll_once(fd, POLLIN, (int)remaining);
        if (!(events & POLLIN)) {
            return;
        }
        struct packet_record candidate;
        receive_record(fd, &candidate, 0);
        if ((size_t)candidate.length >= needle_len &&
            contains_bytes(candidate.data, (size_t)candidate.length, needle,
                           needle_len)) {
            fail_message("unexpected-extra-record", "matching-packet");
        }
    }
}

static void require_packet_address(const struct packet_record *record,
                                   unsigned int protocol, int packet_type,
                                   const unsigned char source[6]) {
    require_true(record->address_length == sizeof(struct sockaddr_ll),
                 "packet-address-length");
    require_true(record->address.sll_family == AF_PACKET,
                 "packet-address-family");
    require_true(ntohs(record->address.sll_protocol) == protocol,
                 "packet-address-protocol");
    require_true(record->address.sll_ifindex == (int)loopback_index,
                 "packet-address-interface");
    require_true(record->address.sll_hatype == ARPHRD_LOOPBACK,
                 "packet-address-hardware");
    require_true(record->address.sll_pkttype == packet_type,
                 "packet-address-type");
    require_true(record->address.sll_halen == 6, "packet-address-halen");
    require_true(memcmp(record->address.sll_addr, source, 6) == 0,
                 "packet-address-source");
}

static void require_empty_nonblocking(int fd, const char *stage) {
    unsigned char byte;
    errno = 0;
    ssize_t result = recv(fd, &byte, sizeof(byte), MSG_DONTWAIT);
    if (result != -1 || (errno != EAGAIN && errno != EWOULDBLOCK)) {
        fail_value(stage, result, -1);
    }
}

static void drop_all_capabilities(void) {
    struct __user_cap_header_struct header = {
        .version = _LINUX_CAPABILITY_VERSION_3,
        .pid = 0,
    };
    struct __user_cap_data_struct data[2];
    memset(data, 0, sizeof(data));
    if (syscall(SYS_capset, &header, data) != 0) {
        fail_message("capability-order", "capset");
    }
}

static void expect_socket_error(int type, unsigned int protocol,
                                int expected_errno, const char *stage) {
    errno = 0;
    int fd = socket(AF_PACKET, type, htons((uint16_t)protocol));
    if (fd >= 0) {
        close(fd);
        fail_value(stage, fd, -1);
    }
    if (errno != expected_errno) {
        fail_value(stage, errno, expected_errno);
    }
}

static void expect_socketpair_error(int type, unsigned int protocol,
                                    int expected_errno, const char *stage) {
    int descriptors[2] = {-1, -1};
    errno = 0;
    int result = socketpair(AF_PACKET, type, htons((uint16_t)protocol),
                            descriptors);
    if (result == 0) {
        close(descriptors[0]);
        close(descriptors[1]);
        fail_value(stage, result, -1);
    }
    if (errno != expected_errno) {
        fail_value(stage, errno, expected_errno);
    }
}

static void require_call_error(int result, int expected_errno,
                               const char *stage) {
    if (result != -1 || errno != expected_errno) {
        fail_value(stage, result, -1);
    }
}

static void test_control_errors(void) {
    expect_socketpair_error(SOCK_RAW, ETH_P_IP, EOPNOTSUPP,
                            "control-socketpair-raw");
    expect_socketpair_error(SOCK_DGRAM, ETH_P_IP, EOPNOTSUPP,
                            "control-socketpair-dgram");
    expect_socketpair_error(SOCK_STREAM, ETH_P_IP, ESOCKTNOSUPPORT,
                            "control-socketpair-stream");

    int fd = packet_socket(SOCK_RAW, 0);
    struct sockaddr_ll address;
    memset(&address, 0, sizeof(address));
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(ETH_P_IP);
    address.sll_ifindex = (int)loopback_index;

    errno = 0;
    require_call_error(connect(fd, (const struct sockaddr *)&address,
                               sizeof(address)),
                       EOPNOTSUPP, "control-connect");
    errno = 0;
    require_call_error(listen(fd, 1), EOPNOTSUPP, "control-listen");
    errno = 0;
    require_call_error(shutdown(fd, SHUT_RDWR), EOPNOTSUPP,
                       "control-shutdown");
    socklen_t length = sizeof(address);
    errno = 0;
    require_call_error(getpeername(fd, (struct sockaddr *)&address, &length),
                       EOPNOTSUPP, "control-getpeername");
    errno = 0;
    require_call_error(accept(fd, NULL, NULL), EOPNOTSUPP,
                       "control-accept");

    address.sll_family = AF_PACKET;
    address.sll_ifindex = -1;
    errno = 0;
    require_call_error(bind(fd, (const struct sockaddr *)&address,
                            sizeof(address)),
                       ENODEV, "control-bind-negative-ifindex");
    address.sll_ifindex = INT_MAX;
    errno = 0;
    require_call_error(bind(fd, (const struct sockaddr *)&address,
                            sizeof(address)),
                       ENODEV, "control-bind-missing-ifindex");
    address.sll_ifindex = (int)loopback_index;
    errno = 0;
    require_call_error(bind(fd, (const struct sockaddr *)&address,
                            sizeof(address) - 1),
                       EINVAL, "control-bind-short-name");
    address.sll_family = AF_INET;
    errno = 0;
    require_call_error(bind(fd, (const struct sockaddr *)&address,
                            sizeof(address)),
                       EINVAL, "control-bind-family");
    close_checked(fd, "control-bind-close");

    fd = bound_packet_socket(SOCK_DGRAM, CUSTOM_PROTOCOL, loopback_index);
    unsigned char token[16];
    make_token(token, 0, 1);
    memset(&address, 0, sizeof(address));
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(CUSTOM_PROTOCOL);
    address.sll_ifindex = (int)loopback_index;
    address.sll_halen = 6;

    void *inaccessible = mmap(NULL, 4096, PROT_NONE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (inaccessible == MAP_FAILED) {
        fail_message("control-send-order", "mmap");
    }
    address.sll_ifindex = INT_MAX;
    errno = 0;
    require_call_error((int)sendto(fd, inaccessible, sizeof(token), 0,
                                   (const struct sockaddr *)&address,
                                   sizeof(address)),
                       ENXIO, "control-send-ifindex-before-payload-fault");
    address.sll_ifindex = (int)loopback_index;
    errno = 0;
    require_call_error((int)sendto(fd, inaccessible, sizeof(token), 0,
                                   (const struct sockaddr *)&address,
                                   sizeof(address)),
                       EFAULT, "control-send-valid-ifindex-payload-fault");
    if (munmap(inaccessible, 4096) != 0) {
        fail_message("control-send-order", "munmap");
    }

    errno = 0;
    require_call_error((int)sendto(fd, token, sizeof(token), 0,
                                   (const struct sockaddr *)&address,
                                   sizeof(address) - 1),
                       EINVAL, "control-send-short-name");
    address.sll_family = AF_INET;
    errno = 0;
    ssize_t sent = sendto(fd, token, sizeof(token), 0,
                          (const struct sockaddr *)&address, sizeof(address));
    if (sent != (ssize_t)sizeof(token)) {
        fail_value("control-send-family-ignored", sent, (long)sizeof(token));
    }
    printf("THEKERNEL_PACKET_CONTROL_BOUNDARY bind_wrong_family_errno=%d "
           "bind_missing_ifindex_errno=%d send_wrong_family_accepted=1 "
           "short_name_errno=%d send_ifindex_before_fault_errno=%d "
           "socketpair_errno=%d\n",
           EINVAL, ENODEV, EINVAL, ENXIO, EOPNOTSUPP);
    fflush(stdout);
    close_checked(fd, "control-send-close");
}

static void test_create(void) {
    int raw = packet_socket(SOCK_RAW, ETH_P_IP);
    int datagram = packet_socket(SOCK_DGRAM, ETH_P_IP);
    int disabled = packet_socket(SOCK_DGRAM, 0);
    close_checked(raw, "create-raw-close");
    close_checked(datagram, "create-dgram-close");
    close_checked(disabled, "create-disabled-close");

    expect_socket_error(0x7f, ETH_P_IP, EINVAL, "create-invalid-type");
    expect_socket_error(SOCK_STREAM, ETH_P_IP, ESOCKTNOSUPPORT,
                        "create-stream-with-capability");

    if (linux_host_mode) {
        pid_t child = fork();
        if (child < 0) {
            fail_message("capability-order", "fork");
        }
        if (child == 0) {
            drop_all_capabilities();
            expect_socket_error(0x7f, ETH_P_IP, EINVAL,
                                "create-invalid-type-without-capability");
            expect_socket_error(SOCK_RAW, ETH_P_IP, EPERM,
                                "create-raw-without-capability");
            expect_socket_error(SOCK_DGRAM, ETH_P_IP, EPERM,
                                "create-dgram-without-capability");
            expect_socket_error(SOCK_STREAM, ETH_P_IP, EPERM,
                                "create-stream-without-capability");
            expect_socketpair_error(0x7f, ETH_P_IP, EINVAL,
                                    "socketpair-invalid-type-without-capability");
            expect_socketpair_error(SOCK_RAW, ETH_P_IP, EPERM,
                                    "socketpair-raw-without-capability");
            expect_socketpair_error(SOCK_STREAM, ETH_P_IP, EPERM,
                                    "socketpair-stream-without-capability");
            _exit(EXIT_SUCCESS);
        }
        int status = 0;
        if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
            WEXITSTATUS(status) != 0) {
            fail_message("capability-order", "child-status");
        }
    }
    test_control_errors();
    marker("THEKERNEL_PACKET_CREATE_OK");
}

static void require_udp_views(const struct packet_record *raw,
                              const struct packet_record *datagram,
                              int packet_type) {
    static const unsigned char zero_address[6] = {0};
    require_packet_address(raw, ETH_P_IP, packet_type, zero_address);
    require_packet_address(datagram, ETH_P_IP, packet_type, zero_address);
    require_true(raw->length == datagram->length + ETH_HLEN,
                 "receive-view-length");
    require_true(raw->length >= ETH_HLEN, "receive-raw-minimum");
    static const unsigned char loopback_header[ETH_HLEN] = {
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08, 0x00,
    };
    require_true(memcmp(raw->data, loopback_header, ETH_HLEN) == 0,
                 "receive-raw-header");
    require_true(memcmp(raw->data + ETH_HLEN, datagram->data,
                        (size_t)datagram->length) == 0,
                 "receive-view-data");
}

static void require_getname(int fd, unsigned int protocol,
                            unsigned int ifindex) {
    struct sockaddr_ll address;
    memset(&address, 0xa5, sizeof(address));
    socklen_t length = sizeof(address);
    if (getsockname(fd, (struct sockaddr *)&address, &length) != 0) {
        fail_message("packet-getsockname", "call");
    }
    socklen_t expected_length =
        (socklen_t)(offsetof(struct sockaddr_ll, sll_addr) +
                    (ifindex == 0 ? 0 : 6));
    require_true(length == expected_length, "packet-getsockname-length");
    require_true(address.sll_family == AF_PACKET,
                 "packet-getsockname-family");
    require_true(ntohs(address.sll_protocol) == protocol,
                 "packet-getsockname-protocol");
    require_true(address.sll_ifindex == (int)ifindex,
                 "packet-getsockname-interface");
    if (ifindex == 0) {
        require_true(address.sll_hatype == 0 && address.sll_halen == 0,
                     "packet-getsockname-wildcard-link");
    } else {
        require_true(address.sll_hatype == ARPHRD_LOOPBACK,
                     "packet-getsockname-hardware");
        require_true(address.sll_halen == 6, "packet-getsockname-halen");
    }
    const unsigned char *raw = (const unsigned char *)&address;
    for (size_t offset = length; offset < sizeof(address); ++offset) {
        require_true(raw[offset] == 0xa5,
                     "packet-getsockname-tail-untouched");
    }
}

static void test_receive_and_bind(void) {
    int raw_exact = bound_packet_socket(SOCK_RAW, ETH_P_IP, loopback_index);
    int raw_all = bound_packet_socket(SOCK_RAW, ETH_P_ALL, loopback_index);
    int dgram_exact =
        bound_packet_socket(SOCK_DGRAM, ETH_P_IP, loopback_index);
    int dgram_all =
        bound_packet_socket(SOCK_DGRAM, ETH_P_ALL, loopback_index);

    require_empty_nonblocking(raw_exact, "receive-empty-nonblocking");
    short initial = poll_once(raw_exact, POLLIN | POLLOUT, 0);
    require_true(!(initial & POLLIN) && (initial & POLLOUT),
                 "receive-initial-poll");

    struct udp_pair udp;
    udp_pair_open(&udp);
    unsigned char token[16];
    make_token(token, 1, 1);
    udp_send_and_drain(&udp, token);

    short ready = poll_once(raw_exact, POLLIN | POLLOUT, WAIT_MILLISECONDS);
    require_true((ready & POLLIN) && (ready & POLLOUT), "receive-ready-poll");

    struct packet_record raw_exact_records[1];
    struct packet_record raw_all_records[2];
    struct packet_record dgram_exact_records[1];
    struct packet_record dgram_all_records[2];
    collect_records(raw_exact, token, sizeof(token), raw_exact_records, 1);
    collect_records(raw_all, token, sizeof(token), raw_all_records, 2);
    collect_records(dgram_exact, token, sizeof(token), dgram_exact_records, 1);
    collect_records(dgram_all, token, sizeof(token), dgram_all_records, 2);
    require_no_matching_record(raw_exact, token, sizeof(token));
    require_no_matching_record(raw_all, token, sizeof(token));
    require_no_matching_record(dgram_exact, token, sizeof(token));
    require_no_matching_record(dgram_all, token, sizeof(token));

    require_udp_views(&raw_exact_records[0], &dgram_exact_records[0],
                      PACKET_HOST);
    require_udp_views(&raw_all_records[0], &dgram_all_records[0],
                      PACKET_OUTGOING);
    require_udp_views(&raw_all_records[1], &dgram_all_records[1], PACKET_HOST);
    require_true(raw_exact_records[0].length == raw_all_records[1].length &&
                     memcmp(raw_exact_records[0].data, raw_all_records[1].data,
                            (size_t)raw_exact_records[0].length) == 0,
                 "receive-exact-host-copy");

    require_empty_nonblocking(raw_exact, "receive-consumed-nonblocking");
    short drained = poll_once(raw_exact, POLLIN | POLLOUT, 0);
    require_true(!(drained & POLLIN) && (drained & POLLOUT),
                 "receive-drained-poll");

    int rebound = packet_socket(SOCK_RAW, 0);
    require_getname(rebound, 0, 0);
    bind_packet(rebound, ETH_P_IP, loopback_index);
    require_getname(rebound, ETH_P_IP, loopback_index);
    make_token(token, 1, 2);
    udp_send_and_drain(&udp, token);
    struct packet_record rebound_record[1];
    collect_records(rebound, token, sizeof(token), rebound_record, 1);
    require_packet_address(&rebound_record[0], ETH_P_IP, PACKET_HOST,
                           (const unsigned char[6]){0});

    bind_packet(rebound, 0, 0);
    require_getname(rebound, ETH_P_IP, 0);
    printf("THEKERNEL_PACKET_NAME_BOUNDARY unbound_length=%zu "
           "bound_loopback_length=%zu wildcard_length=%zu\n",
           offsetof(struct sockaddr_ll, sll_addr),
           offsetof(struct sockaddr_ll, sll_addr) + 6,
           offsetof(struct sockaddr_ll, sll_addr));
    fflush(stdout);
    make_token(token, 1, 3);
    udp_send_and_drain(&udp, token);
    collect_records(rebound, token, sizeof(token), rebound_record, 1);
    require_packet_address(&rebound_record[0], ETH_P_IP, PACKET_HOST,
                           (const unsigned char[6]){0});

    close_checked(rebound, "receive-rebound-close");
    udp_pair_close(&udp);
    close_checked(raw_exact, "receive-raw-exact-close");
    close_checked(raw_all, "receive-raw-all-close");
    close_checked(dgram_exact, "receive-dgram-exact-close");
    close_checked(dgram_all, "receive-dgram-all-close");
}

static ssize_t receive_small(int fd, void *buffer, size_t length, int flags,
                             int *output_flags) {
    struct sockaddr_ll address;
    struct iovec iov = {.iov_base = buffer, .iov_len = length};
    struct msghdr message = {
        .msg_name = &address,
        .msg_namelen = sizeof(address),
        .msg_iov = &iov,
        .msg_iovlen = 1,
    };
    ssize_t result = recvmsg(fd, &message, flags);
    *output_flags = message.msg_flags;
    return result;
}

static int fresh_udp_packet(struct udp_pair *udp, unsigned char token[16],
                            unsigned char test_case) {
    int fd = bound_packet_socket(SOCK_DGRAM, ETH_P_IP, loopback_index);
    make_token(token, 2, test_case);
    udp_send_and_drain(udp, token);
    require_true(poll_once(fd, POLLIN, WAIT_MILLISECONDS) & POLLIN,
                 "trunc-poll");
    return fd;
}

static void test_truncation(void) {
    struct udp_pair udp;
    udp_pair_open(&udp);
    unsigned char token[16];
    unsigned char small[8];
    struct packet_record full[1];
    int output_flags = 0;

    int fd = fresh_udp_packet(&udp, token, 1);
    ssize_t result = receive_small(fd, small, sizeof(small), MSG_PEEK,
                                   &output_flags);
    require_true(result == (ssize_t)sizeof(small) &&
                     (output_flags & MSG_TRUNC),
                 "trunc-peek-short");
    collect_records(fd, token, sizeof(token), full, 1);
    ssize_t complete_length = full[0].length;
    close_checked(fd, "trunc-peek-close");

    fd = fresh_udp_packet(&udp, token, 2);
    result = receive_small(fd, small, sizeof(small), MSG_PEEK | MSG_TRUNC,
                           &output_flags);
    require_true(result == complete_length && (output_flags & MSG_TRUNC),
                 "trunc-peek-full-length");
    collect_records(fd, token, sizeof(token), full, 1);
    require_true(full[0].length == complete_length,
                 "trunc-peek-retained-length");
    close_checked(fd, "trunc-peek-full-close");

    fd = fresh_udp_packet(&udp, token, 3);
    result = receive_small(fd, small, sizeof(small), 0, &output_flags);
    require_true(result == (ssize_t)sizeof(small) &&
                     (output_flags & MSG_TRUNC),
                 "trunc-ordinary-short");
    require_empty_nonblocking(fd, "trunc-ordinary-consumed");
    close_checked(fd, "trunc-ordinary-close");

    fd = fresh_udp_packet(&udp, token, 4);
    result = receive_small(fd, small, sizeof(small), MSG_TRUNC, &output_flags);
    require_true(result == complete_length && (output_flags & MSG_TRUNC),
                 "trunc-ordinary-full-length");
    require_empty_nonblocking(fd, "trunc-full-consumed");
    close_checked(fd, "trunc-full-close");

    udp_pair_close(&udp);
}

static void test_receive(void) {
    test_receive_and_bind();
    test_truncation();

    struct udp_pair udp;
    udp_pair_open(&udp);
    unsigned char token[16];
    unsigned char byte = 0;
    struct packet_record record[1];

    int fd = fresh_udp_packet(&udp, token, 5);
    ssize_t result = read(fd, &byte, 0);
    require_true(result == 0, "zero-read-result");
    require_true(poll_once(fd, POLLIN, 0) & POLLIN,
                 "zero-read-retains-record");
    collect_records(fd, token, sizeof(token), record, 1);
    require_empty_nonblocking(fd, "zero-read-followup-consumed");
    close_checked(fd, "zero-read-close");

    fd = fresh_udp_packet(&udp, token, 6);
    result = recv(fd, &byte, 0, MSG_DONTWAIT);
    require_true(result == 0, "zero-recv-result");
    require_empty_nonblocking(fd, "zero-recv-consumes-record");
    close_checked(fd, "zero-recv-close");
    udp_pair_close(&udp);

    printf("THEKERNEL_PACKET_ZERO_LENGTH_BOUNDARY read_retains=1 recv_consumes=1\n");
    fflush(stdout);
    marker("THEKERNEL_PACKET_RECEIVE_OK");
}

static void test_fault_ownership(void) {
    struct udp_pair udp;
    udp_pair_open(&udp);
    void *inaccessible = mmap(NULL, 4096, PROT_NONE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (inaccessible == MAP_FAILED) {
        fail_message("fault-ownership", "mmap");
    }
    unsigned char token[16];
    struct packet_record retained[1];

    int fd = fresh_udp_packet(&udp, token, 1);
    errno = 0;
    ssize_t result = recv(fd, inaccessible, 64, 0);
    if (result != -1 || errno != EFAULT) {
        fail_value("fault-ordinary", result, -1);
    }
    require_empty_nonblocking(fd, "fault-ordinary-consumed");
    close_checked(fd, "fault-ordinary-close");

    fd = fresh_udp_packet(&udp, token, 2);
    errno = 0;
    result = recv(fd, inaccessible, 64, MSG_PEEK);
    if (result != -1 || errno != EFAULT) {
        fail_value("fault-peek", result, -1);
    }
    collect_records(fd, token, sizeof(token), retained, 1);
    require_empty_nonblocking(fd, "fault-peek-followup-consumed");
    close_checked(fd, "fault-peek-close");

    if (munmap(inaccessible, 4096) != 0) {
        fail_message("fault-ownership", "munmap");
    }
    udp_pair_close(&udp);
    marker("THEKERNEL_PACKET_FAULT_OWNERSHIP_OK");
}

static void fill_packet_destination(struct sockaddr_ll *address,
                                    unsigned int protocol,
                                    const unsigned char destination[6]) {
    memset(address, 0, sizeof(*address));
    address->sll_family = AF_PACKET;
    address->sll_protocol = htons((uint16_t)protocol);
    address->sll_ifindex = (int)loopback_index;
    address->sll_halen = 6;
    if (destination != NULL) {
        memcpy(address->sll_addr, destination, 6);
    }
}

static void require_wire_frame(const struct packet_record *record,
                               const unsigned char destination[6],
                               const unsigned char source[6],
                               unsigned int header_protocol,
                               const unsigned char *payload,
                               size_t payload_length) {
    require_true(record->length == (ssize_t)(ETH_HLEN + payload_length),
                 "send-wire-length");
    require_true(memcmp(record->data, destination, 6) == 0,
                 "send-wire-destination");
    require_true(memcmp(record->data + 6, source, 6) == 0,
                 "send-wire-source");
    require_true(record->data[12] == (unsigned char)(header_protocol >> 8) &&
                     record->data[13] == (unsigned char)header_protocol,
                 "send-wire-protocol");
    require_true(memcmp(record->data + ETH_HLEN, payload, payload_length) == 0,
                 "send-wire-payload");
}

static void format_address(const unsigned char address[6], char output[18]) {
    int written = snprintf(output, 18, "%02x:%02x:%02x:%02x:%02x:%02x",
                           address[0], address[1], address[2], address[3],
                           address[4], address[5]);
    require_true(written == 17, "format-link-address");
}

static void test_raw_send(void) {
    static const unsigned char destination[6] = {0x02, 0x10, 0x20,
                                                  0x30, 0x40, 0x50};
    static const unsigned char source[6] = {0x02, 0xa1, 0xb2,
                                             0xc3, 0xd4, 0xe5};
    unsigned char token[16];
    make_token(token, 3, 1);
    unsigned char frame[ETH_HLEN + sizeof(token)];
    memcpy(frame, destination, 6);
    memcpy(frame + 6, source, 6);
    frame[12] = (unsigned char)(CUSTOM_PROTOCOL >> 8);
    frame[13] = (unsigned char)CUSTOM_PROTOCOL;
    memcpy(frame + ETH_HLEN, token, sizeof(token));

    int observer_raw =
        bound_packet_socket(SOCK_RAW, ETH_P_ALL, loopback_index);
    int observer_dgram =
        bound_packet_socket(SOCK_DGRAM, ETH_P_ALL, loopback_index);
    int sender = bound_packet_socket(SOCK_RAW, ETH_P_ALL, loopback_index);
    struct sockaddr_ll send_address;
    fill_packet_destination(&send_address, CUSTOM_PROTOCOL, destination);
    ssize_t sent = sendto(sender, frame, sizeof(frame), 0,
                          (const struct sockaddr *)&send_address,
                          sizeof(send_address));
    if (sent != (ssize_t)sizeof(frame)) {
        fail_value("send-raw", sent, (long)sizeof(frame));
    }

    struct packet_record raw_records[2];
    struct packet_record dgram_records[2];
    struct packet_record source_records[1];
    collect_records(observer_raw, token, sizeof(token), raw_records, 2);
    collect_records(observer_dgram, token, sizeof(token), dgram_records, 2);
    collect_records(sender, token, sizeof(token), source_records, 1);
    require_no_matching_record(sender, token, sizeof(token));
    require_wire_frame(&raw_records[0], destination, source, CUSTOM_PROTOCOL,
                       token, sizeof(token));
    require_wire_frame(&raw_records[1], destination, source, CUSTOM_PROTOCOL,
                       token, sizeof(token));
    require_true(dgram_records[0].length == (ssize_t)sizeof(token) &&
                     dgram_records[1].length == (ssize_t)sizeof(token) &&
                     memcmp(dgram_records[0].data, token, sizeof(token)) == 0 &&
                     memcmp(dgram_records[1].data, token, sizeof(token)) == 0,
                 "send-raw-cooked-view");
    require_packet_address(&raw_records[0], CUSTOM_PROTOCOL, PACKET_OUTGOING,
                           source);
    require_packet_address(&raw_records[1], CUSTOM_PROTOCOL, PACKET_OTHERHOST,
                           source);
    require_packet_address(&source_records[0], CUSTOM_PROTOCOL,
                           PACKET_OTHERHOST, source);

    close_checked(sender, "send-raw-source-close");
    close_checked(observer_dgram, "send-raw-dgram-close");
    close_checked(observer_raw, "send-raw-observer-close");
}

static void test_dgram_send(void) {
    static const unsigned char destination[6] = {0x02, 0x11, 0x22,
                                                  0x33, 0x44, 0x55};
    static const unsigned char source[6] = {0};
    unsigned char token[16];
    make_token(token, 3, 2);
    int observer = bound_packet_socket(SOCK_RAW, ETH_P_ALL, loopback_index);
    int sender = bound_packet_socket(SOCK_DGRAM, ETH_P_ALL, loopback_index);
    struct sockaddr_ll send_address;
    fill_packet_destination(&send_address, CUSTOM_PROTOCOL, destination);
    ssize_t sent = sendto(sender, token, sizeof(token), 0,
                          (const struct sockaddr *)&send_address,
                          sizeof(send_address));
    if (sent != (ssize_t)sizeof(token)) {
        fail_value("send-dgram", sent, (long)sizeof(token));
    }
    struct packet_record observed[2];
    struct packet_record source_records[1];
    collect_records(observer, token, sizeof(token), observed, 2);
    collect_records(sender, token, sizeof(token), source_records, 1);
    require_no_matching_record(sender, token, sizeof(token));
    require_wire_frame(&observed[0], destination, source, CUSTOM_PROTOCOL,
                       token, sizeof(token));
    require_wire_frame(&observed[1], destination, source, CUSTOM_PROTOCOL,
                       token, sizeof(token));
    require_packet_address(&observed[0], CUSTOM_PROTOCOL, PACKET_OUTGOING,
                           source);
    require_packet_address(&observed[1], CUSTOM_PROTOCOL, PACKET_OTHERHOST,
                           source);
    require_packet_address(&source_records[0], CUSTOM_PROTOCOL,
                           PACKET_OTHERHOST, source);
    close_checked(sender, "send-dgram-source-close");
    close_checked(observer, "send-dgram-observer-close");
}

static void print_send_boundary(const char *test_case,
                                const struct packet_record records[2]) {
    char destination[18];
    char source[18];
    format_address(records[0].data, destination);
    format_address(records[0].data + 6, source);
    unsigned int header_protocol =
        (unsigned int)records[0].data[12] << 8 | records[0].data[13];
    printf("THEKERNEL_PACKET_SEND_BOUNDARY case=%s dst=%s src=%s "
           "header_protocol=0x%04x outgoing_protocol=0x%04x "
           "ingress_protocol=0x%04x\n",
           test_case, destination, source, header_protocol,
           ntohs(records[0].address.sll_protocol),
           ntohs(records[1].address.sll_protocol));
    fflush(stdout);
}

static void test_dgram_send_boundaries(void) {
    static const unsigned char zero[6] = {0};
    static const unsigned char destination[6] = {0x02, 0x66, 0x77,
                                                  0x88, 0x99, 0xaa};
    unsigned char token[16];
    struct packet_record observed[2];

    int observer = bound_packet_socket(SOCK_RAW, ETH_P_ALL, loopback_index);
    int sender =
        bound_packet_socket(SOCK_DGRAM, CUSTOM_PROTOCOL, loopback_index);
    make_token(token, 3, 3);
    ssize_t sent = write(sender, token, sizeof(token));
    if (sent != (ssize_t)sizeof(token)) {
        fail_value("send-dgram-bound-write", sent, (long)sizeof(token));
    }
    collect_records(observer, token, sizeof(token), observed, 2);
    require_wire_frame(&observed[0], zero, zero, CUSTOM_PROTOCOL, token,
                       sizeof(token));
    require_wire_frame(&observed[1], zero, zero, CUSTOM_PROTOCOL, token,
                       sizeof(token));
    print_send_boundary("bound-write", observed);
    close_checked(sender, "send-bound-write-source-close");
    close_checked(observer, "send-bound-write-observer-close");

    observer = bound_packet_socket(SOCK_RAW, ETH_P_ALL, loopback_index);
    sender = bound_packet_socket(SOCK_DGRAM, CUSTOM_PROTOCOL, loopback_index);
    struct sockaddr_ll send_address;
    fill_packet_destination(&send_address, 0, destination);
    make_token(token, 3, 4);
    sent = sendto(sender, token, sizeof(token), 0,
                  (const struct sockaddr *)&send_address,
                  sizeof(send_address));
    if (sent != (ssize_t)sizeof(token)) {
        fail_value("send-dgram-zero-protocol", sent, (long)sizeof(token));
    }
    collect_records(observer, token, sizeof(token), observed, 2);
    require_wire_frame(&observed[0], destination, zero, 0, token,
                       sizeof(token));
    require_wire_frame(&observed[1], destination, zero, 0, token,
                       sizeof(token));
    require_true(ntohs(observed[0].address.sll_protocol) == 0,
                 "send-zero-outgoing-protocol");
    require_true(ntohs(observed[1].address.sll_protocol) == ETH_P_802_2,
                 "send-zero-ingress-protocol");
    print_send_boundary("sendto-zero-protocol", observed);
    close_checked(sender, "send-zero-source-close");
    close_checked(observer, "send-zero-observer-close");

    observer = bound_packet_socket(SOCK_RAW, ETH_P_ALL, loopback_index);
    sender = bound_packet_socket(SOCK_DGRAM, CUSTOM_PROTOCOL, loopback_index);
    struct extended_sockaddr_ll extended_address;
    memset(&extended_address, 0, sizeof(extended_address));
    fill_packet_destination(&extended_address.address, CUSTOM_PROTOCOL,
                            destination);
    extended_address.address.sll_halen = 9;
    extended_address.address.sll_addr[6] = 0xde;
    extended_address.address.sll_addr[7] = 0xad;
    extended_address.ninth_address_byte = 0xbe;
    const socklen_t extended_length =
        (socklen_t)(offsetof(struct sockaddr_ll, sll_addr) + 9);
    require_true(extended_length <= sizeof(extended_address),
                 "send-extended-address-storage");
    make_token(token, 3, 5);
    errno = 0;
    sent = sendto(sender, token, sizeof(token), 0,
                  (const struct sockaddr *)&extended_address,
                  sizeof(struct sockaddr_ll));
    if (sent != -1 || errno != EINVAL) {
        fail_value("send-extended-address-short", sent, -1);
    }
    require_no_matching_record(observer, token, sizeof(token));
    sent = sendto(sender, token, sizeof(token), 0,
                  (const struct sockaddr *)&extended_address,
                  extended_length);
    if (sent != (ssize_t)sizeof(token)) {
        fail_value("send-extended-address", sent, (long)sizeof(token));
    }
    collect_records(observer, token, sizeof(token), observed, 2);
    require_wire_frame(&observed[0], destination, zero, CUSTOM_PROTOCOL,
                       token, sizeof(token));
    require_wire_frame(&observed[1], destination, zero, CUSTOM_PROTOCOL,
                       token, sizeof(token));
    require_packet_address(&observed[0], CUSTOM_PROTOCOL, PACKET_OUTGOING,
                           zero);
    require_packet_address(&observed[1], CUSTOM_PROTOCOL, PACKET_OTHERHOST,
                           zero);
    print_send_boundary("extended-halen9", observed);
    close_checked(sender, "send-extended-source-close");
    close_checked(observer, "send-extended-observer-close");
}

static void test_send_flags(void) {
    static const unsigned char destination[6] = {0x02, 0x71, 0x72,
                                                  0x73, 0x74, 0x75};
    static const unsigned char source[6] = {0};
    static const struct {
        int flag;
        const char *name;
    } cases[] = {
        {MSG_OOB, "OOB"},
        {MSG_MORE, "MORE"},
        {MSG_DONTROUTE, "DONTROUTE"},
        {MSG_EOR, "EOR"},
        {MSG_CONFIRM, "CONFIRM"},
        {MSG_NOSIGNAL, "NOSIGNAL"},
    };

    for (size_t index = 0; index < sizeof(cases) / sizeof(cases[0]); ++index) {
        int observer =
            bound_packet_socket(SOCK_RAW, ETH_P_ALL, loopback_index);
        int sender =
            bound_packet_socket(SOCK_DGRAM, CUSTOM_PROTOCOL, loopback_index);
        struct sockaddr_ll send_address;
        fill_packet_destination(&send_address, CUSTOM_PROTOCOL, destination);
        unsigned char token[16];
        make_token(token, 3, (unsigned char)(10 + index));
        ssize_t sent = sendto(sender, token, sizeof(token), cases[index].flag,
                              (const struct sockaddr *)&send_address,
                              sizeof(send_address));
        if (sent != (ssize_t)sizeof(token)) {
            fail_value(cases[index].name, sent, (long)sizeof(token));
        }
        struct packet_record observed[2];
        collect_records(observer, token, sizeof(token), observed, 2);
        require_wire_frame(&observed[0], destination, source, CUSTOM_PROTOCOL,
                           token, sizeof(token));
        require_wire_frame(&observed[1], destination, source, CUSTOM_PROTOCOL,
                           token, sizeof(token));
        close_checked(sender, "send-flags-source-close");
        close_checked(observer, "send-flags-observer-close");
    }

    printf("THEKERNEL_PACKET_SEND_FLAGS_BOUNDARY "
           "accepted=OOB,MORE,DONTROUTE,EOR,CONFIRM,NOSIGNAL\n");
    fflush(stdout);
    marker("THEKERNEL_PACKET_SEND_FLAGS_OK");
}

static void test_send(void) {
    test_raw_send();
    test_dgram_send();
    test_dgram_send_boundaries();
    test_send_flags();
    marker("THEKERNEL_PACKET_SEND_OK");
}

static void attach_metadata_filter(int fd, uint32_t protocol,
                                   uint32_t ifindex) {
    /* SKF_AD_PROTOCOL is skb->protocol in host order (ETH_P_IP), while
     * sockaddr_ll.sll_protocol below is the network-order UAPI field. */
    struct sock_filter instructions[] = {
        {
            BPF_LD | BPF_W | BPF_ABS,
            0,
            0,
            (uint32_t)(SKF_AD_OFF + SKF_AD_PROTOCOL),
        },
        {BPF_JMP | BPF_JEQ | BPF_K, 0, 3, protocol},
        {
            BPF_LD | BPF_W | BPF_ABS,
            0,
            0,
            (uint32_t)(SKF_AD_OFF + SKF_AD_IFINDEX),
        },
        {BPF_JMP | BPF_JEQ | BPF_K, 0, 1, ifindex},
        {BPF_RET | BPF_K, 0, 0, UINT32_MAX},
        {BPF_RET | BPF_K, 0, 0, 0},
    };
    struct sock_fprog program = {
        .len = (unsigned short)(sizeof(instructions) / sizeof(instructions[0])),
        .filter = instructions,
    };
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &program,
                   sizeof(program)) != 0) {
        fail_message("cbpf-metadata-attach", "filter");
    }
}

static void require_metadata_counter_proof(enum cbpf_executor_mode mode,
                                           const struct cbpf_counters *before,
                                           const struct cbpf_counters *after) {
    struct cbpf_counters delta;
    memset(&delta, 0, sizeof(delta));
    bool valid =
        subtract_counter(after->published, before->published, &delta.published) &&
        subtract_counter(after->native_executed, before->native_executed,
                         &delta.native_executed) &&
        subtract_counter(after->interpreter_executed,
                         before->interpreter_executed,
                         &delta.interpreter_executed) &&
        subtract_counter(after->fallback_policy_interpreter,
                         before->fallback_policy_interpreter,
                         &delta.fallback_policy_interpreter) &&
        subtract_counter(after->fallback_translation,
                         before->fallback_translation,
                         &delta.fallback_translation) &&
        subtract_counter(after->fallback_publication,
                         before->fallback_publication,
                         &delta.fallback_publication) &&
        subtract_counter(after->fallback_owner, before->fallback_owner,
                         &delta.fallback_owner) &&
        subtract_counter(after->fallback_unavailable,
                         before->fallback_unavailable,
                         &delta.fallback_unavailable) &&
        subtract_counter(after->jit_rejected, before->jit_rejected,
                         &delta.jit_rejected);
    if (!valid) {
        fail_message("cbpf-metadata-executor", "counter-regression");
    }
    if (delta.published == 0) {
        fail_message("cbpf-metadata-executor", "no-publication");
    }
    if (mode == CBPF_EXECUTOR_INTERPRETER) {
        if (delta.native_executed != 0 || delta.interpreter_executed == 0 ||
            delta.fallback_policy_interpreter == 0 ||
            delta.fallback_translation != 0 ||
            delta.fallback_publication != 0 || delta.fallback_owner != 0 ||
            delta.fallback_unavailable != 0 || delta.jit_rejected != 0) {
            fail_message("cbpf-metadata-executor", "interpreter-proof");
        }
    } else if (mode == CBPF_EXECUTOR_JIT) {
        if (delta.native_executed == 0 || delta.interpreter_executed != 0 ||
            delta.fallback_policy_interpreter != 0 ||
            delta.fallback_translation != 0 ||
            delta.fallback_publication != 0 || delta.fallback_owner != 0 ||
            delta.fallback_unavailable != 0 || delta.jit_rejected != 0) {
            fail_message("cbpf-metadata-executor", "jit-proof");
        }
    } else if (delta.native_executed == 0 && delta.interpreter_executed == 0) {
        fail_message("cbpf-metadata-executor", "auto-proof");
    }
    printf("THEKERNEL_PACKET_CBPF_METADATA_EXECUTOR mode=%s published=%llu "
           "native=%llu interpreter=%llu fallback_policy_interpreter=%llu "
           "fallback_translation=%llu fallback_publication=%llu "
           "fallback_owner=%llu fallback_unavailable=%llu jit_rejected=%llu "
           "proof=verified\n",
           cbpf_executor_name(mode), delta.published, delta.native_executed,
           delta.interpreter_executed, delta.fallback_policy_interpreter,
           delta.fallback_translation, delta.fallback_publication,
           delta.fallback_owner, delta.fallback_unavailable,
           delta.jit_rejected);
    fflush(stdout);
}

static void run_metadata_lane(enum cbpf_executor_mode mode,
                              bool require_counter_proof,
                              bool control_available,
                              unsigned int lane_number) {
    struct cbpf_counters before;
    struct cbpf_counters after;
    if (control_available && set_cbpf_executor(mode) != 1) {
        fail_message("cbpf-metadata-executor", "control");
    }
    if (require_counter_proof) {
        if (read_cbpf_stats(&before) != 1) {
            fail_message("cbpf-metadata-executor", "control-or-stats");
        }
    }

    struct udp_pair udp;
    udp_pair_open(&udp);
    /* ETH_P_ALL keeps a deliberately wrong ancillary protocol in the
     * selector, so the cBPF branch—not socket binding—performs the drop. */
    int fd = bound_packet_socket(SOCK_RAW, ETH_P_ALL, loopback_index);
    unsigned char token[16];
    struct packet_record records[2];

    attach_metadata_filter(fd, ETH_P_IP, loopback_index);
    make_token(token, 6, (unsigned char)(lane_number * 3U + 1U));
    udp_send_and_drain(&udp, token);
    collect_records(fd, token, sizeof(token), records, 2);
    for (size_t index = 0; index < 2; ++index) {
        require_true(ntohs(records[index].address.sll_protocol) == ETH_P_IP,
                     "cbpf-metadata-protocol-order");
        require_true(records[index].address.sll_ifindex ==
                         (int)loopback_index,
                     "cbpf-metadata-ifindex-match");
    }

    attach_metadata_filter(fd, ETH_P_ARP, loopback_index);
    make_token(token, 6, (unsigned char)(lane_number * 3U + 2U));
    udp_send_and_drain(&udp, token);
    require_no_matching_record(fd, token, sizeof(token));

    unsigned int mismatched_ifindex =
        loopback_index == UINT_MAX ? loopback_index - 1U : loopback_index + 1U;
    attach_metadata_filter(fd, ETH_P_IP, mismatched_ifindex);
    make_token(token, 6, (unsigned char)(lane_number * 3U + 3U));
    udp_send_and_drain(&udp, token);
    require_no_matching_record(fd, token, sizeof(token));

    close_checked(fd, "cbpf-metadata-close");
    udp_pair_close(&udp);
    if (require_counter_proof) {
        if (read_cbpf_stats(&after) != 1) {
            fail_message("cbpf-metadata-executor", "after-stats");
        }
        require_metadata_counter_proof(mode, &before, &after);
    }
}

static void test_classic_filter_metadata(void) {
    char control[BPF_STATS_BUFFER];
    char stats[BPF_STATS_BUFFER];
    int control_state = read_proc_text(BPF_EXECUTOR_CONTROL_PATH, control,
                                       sizeof(control));
    if (control_state < 0) {
        fail_message("cbpf-metadata-executor", "control-read");
    }
    int stats_state = read_proc_text(BPF_STATS_PATH, stats, sizeof(stats));
    if (stats_state < 0) {
        fail_message("cbpf-metadata-executor", "stats-read");
    }
    if (control_state == 1) {
        /* The test-only product control is present only in selected guest
         * builds.  In that lane, both semantic implementations must prove
         * their actual execution path; no filter hot path reads these files. */
        run_metadata_lane(CBPF_EXECUTOR_INTERPRETER, true, true, 1);
        run_metadata_lane(CBPF_EXECUTOR_JIT, true, true, 2);
        if (set_cbpf_executor(CBPF_EXECUTOR_AUTO) != 1) {
            fail_message("cbpf-metadata-executor", "restore-auto");
        }
        printf("THEKERNEL_PACKET_CBPF_METADATA_BOUNDARY protocol=0x%04x "
               "ifindex=%u matching=2 protocol_mismatch_drop=2 "
               "ifindex_mismatch_drop=2 executors=interpreter,jit\n",
               ETH_P_IP, loopback_index);
    } else {
        /* Linux host differential has no TheKernel-only control file.  The
         * product stats file is still usable as a cold before/after proof
         * when the guest exposes it. */
        run_metadata_lane(CBPF_EXECUTOR_AUTO, stats_state == 1, false, 0);
        printf("THEKERNEL_PACKET_CBPF_METADATA_BOUNDARY protocol=0x%04x "
               "ifindex=%u matching=2 protocol_mismatch_drop=2 "
               "ifindex_mismatch_drop=2 executors=auto proof=%s\n", ETH_P_IP,
               loopback_index, stats_state == 1 ? "auto-active" :
                                  "semantic-only");
    }
    fflush(stdout);
    marker("THEKERNEL_PACKET_CBPF_METADATA_OK");
}

static void test_classic_filter(void) {
    test_classic_filter_metadata();
    struct udp_pair udp;
    udp_pair_open(&udp);
    int fd = bound_packet_socket(SOCK_RAW, ETH_P_IP, loopback_index);
    struct sock_filter accept_instruction = {
        BPF_RET | BPF_K, 0, 0, 0xffff,
    };
    struct sock_filter drop_instruction = {
        BPF_RET | BPF_K, 0, 0, 0,
    };
    struct sock_fprog accept_program = {1, &accept_instruction};
    struct sock_fprog drop_program = {1, &drop_instruction};
    struct packet_record record[1];
    unsigned char token[16];

    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, NULL, 0) != -1 ||
        errno != EINVAL) {
        fail_message("cbpf-envelope", "null-len0");
    }
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, NULL, 1) != -1 ||
        errno != EINVAL) {
        fail_message("cbpf-envelope", "null-len1");
    }
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, NULL, 8) != -1 ||
        errno != EFAULT) {
        fail_message("cbpf-envelope", "null-len8");
    }
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, NULL, 4) != -1 ||
        errno != EFAULT) {
        fail_message("cbpf-envelope", "null-len4");
    }
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &accept_program, 8) !=
            -1 ||
        errno != EINVAL) {
        fail_message("cbpf-envelope", "valid-short-length");
    }
    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) {
        fail_message("cbpf-envelope", "page-size");
    }
    void *boundary = mmap(NULL, (size_t)page_size * 2,
                          PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (boundary == MAP_FAILED ||
        mprotect((unsigned char *)boundary + page_size, (size_t)page_size,
                 PROT_NONE) != 0) {
        fail_message("cbpf-envelope", "page-boundary-map");
    }
    unsigned char *boundary_pointer =
        (unsigned char *)boundary + page_size - sizeof(uint32_t);
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, boundary_pointer, 8) !=
            -1 ||
        errno != EINVAL) {
        fail_message("cbpf-envelope", "page-boundary-len8");
    }
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, boundary_pointer, 16) !=
            -1 ||
        errno != EFAULT) {
        fail_message("cbpf-envelope", "page-boundary-len16");
    }
    if (munmap(boundary, (size_t)page_size * 2) != 0) {
        fail_message("cbpf-envelope", "page-boundary-unmap");
    }

    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &accept_program,
                   sizeof(accept_program)) != 0) {
        fail_message("cbpf-attach", "accept");
    }
    make_token(token, 5, 1);
    udp_send_and_drain(&udp, token);
    collect_records(fd, token, sizeof(token), record, 1);
    require_true(record[0].length > 8, "cbpf-accept-full-frame");

    /* A snaplen result intentionally removes the UDP payload token, so the
     * generic token-search collector cannot identify the truncated record.
     * Select this pair's destination port in cBPF before returning 8 bytes,
     * then consume the resulting record directly.  MSH makes the UDP offset
     * correct for any valid IPv4 header length. */
    struct sock_filter snap_instructions[] = {
        {BPF_LDX | BPF_B | BPF_MSH, 0, 0, 14},
        {BPF_LD | BPF_H | BPF_IND, 0, 0, 16},
        {BPF_JMP | BPF_JEQ | BPF_K, 0, 1,
         (uint32_t)ntohs(udp.destination.sin_port)},
        {BPF_RET | BPF_K, 0, 0, 8},
        {BPF_RET | BPF_K, 0, 0, 0},
    };
    struct sock_fprog snap_program = {
        .len = (unsigned short)(sizeof(snap_instructions) /
                                sizeof(snap_instructions[0])),
        .filter = snap_instructions,
    };
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &snap_program,
                   sizeof(snap_program)) != 0) {
        fail_message("cbpf-attach", "snaplen");
    }
    make_token(token, 5, 2);
    udp_send_and_drain(&udp, token);
    short snap_events = poll_once(fd, POLLIN, WAIT_MILLISECONDS);
    require_true((snap_events & POLLIN) != 0, "cbpf-snaplen-ready");
    receive_record(fd, &record[0], 0);
    require_true(record[0].length == 8, "cbpf-snaplen");

    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &drop_program,
                   sizeof(drop_program)) != 0) {
        fail_message("cbpf-attach", "drop");
    }
    make_token(token, 5, 3);
    udp_send_and_drain(&udp, token);
    require_no_matching_record(fd, token, sizeof(token));

    int detach_value = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_DETACH_FILTER, &detach_value,
                   sizeof(detach_value)) != 0) {
        fail_message("cbpf-detach", "attached");
    }
    make_token(token, 5, 4);
    udp_send_and_drain(&udp, token);
    collect_records(fd, token, sizeof(token), record, 1);
    require_true(record[0].length > 8, "cbpf-detach-restores-capture");
    close_checked(fd, "cbpf-detach-close");

    fd = bound_packet_socket(SOCK_RAW, ETH_P_IP, loopback_index);
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_DETACH_FILTER, &detach_value,
                   sizeof(detach_value)) != -1 || errno != ENOENT) {
        fail_message("cbpf-detach", "missing-filter-errno");
    }
    struct sock_filter invalid_instruction = {
        BPF_JMP | BPF_JEQ | BPF_K, 1, 0, 0,
    };
    struct sock_fprog invalid_program = {1, &invalid_instruction};
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &invalid_program,
                   sizeof(invalid_program)) != -1 || errno != EINVAL) {
        fail_message("cbpf-invalid", "jump-errno");
    }
    struct sock_fprog invalid_header = {0, NULL};
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &invalid_header,
                   sizeof(invalid_header)) != -1 || errno != EINVAL) {
        fail_message("cbpf-invalid", "header-errno");
    }
    close_checked(fd, "cbpf-invalid-close");

    fd = bound_packet_socket(SOCK_RAW, ETH_P_IP, loopback_index);
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &accept_program,
                   sizeof(accept_program)) != 0) {
        fail_message("cbpf-lock", "attach");
    }
    int unlock_value = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_LOCK_FILTER, &unlock_value,
                   sizeof(unlock_value)) != 0) {
        fail_message("cbpf-lock", "zero-noop");
    }
    if (setsockopt(fd, SOL_SOCKET, SO_DETACH_FILTER, &detach_value,
                   sizeof(detach_value)) != 0) {
        fail_message("cbpf-lock", "zero-keeps-unlocked");
    }
    close_checked(fd, "cbpf-zero-lock-close");

    fd = bound_packet_socket(SOCK_RAW, ETH_P_IP, loopback_index);
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &accept_program,
                   sizeof(accept_program)) != 0) {
        fail_message("cbpf-lock", "attach-nonzero");
    }
    int lock_value = 1;
    if (setsockopt(fd, SOL_SOCKET, SO_LOCK_FILTER, &lock_value,
                   sizeof(lock_value)) != 0) {
        fail_message("cbpf-lock", "set");
    }
    int locked = 0;
    socklen_t locked_length = sizeof(locked);
    if (getsockopt(fd, SOL_SOCKET, SO_LOCK_FILTER, &locked,
                   &locked_length) != 0 || locked_length != sizeof(locked) ||
        locked != 1) {
        fail_message("cbpf-lock", "get");
    }
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &invalid_header,
                   sizeof(invalid_header)) != -1 || errno != EPERM) {
        fail_message("cbpf-lock", "invalid-header-errno");
    }
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, NULL,
                   sizeof(accept_program)) != -1 || errno != EFAULT) {
        fail_message("cbpf-lock", "null-header-errno");
    }
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_LOCK_FILTER, &unlock_value,
                   sizeof(unlock_value)) != -1 || errno != EPERM) {
        fail_message("cbpf-lock", "zero-is-irreversible");
    }
    locked = 0;
    locked_length = sizeof(locked);
    if (getsockopt(fd, SOL_SOCKET, SO_LOCK_FILTER, &locked,
                   &locked_length) != 0 || locked_length != sizeof(locked) ||
        locked != 1) {
        fail_message("cbpf-lock", "zero-keeps-locked");
    }
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &drop_program,
                   sizeof(drop_program)) != -1 || errno != EPERM) {
        fail_message("cbpf-lock", "replace-errno");
    }
    errno = 0;
    if (setsockopt(fd, SOL_SOCKET, SO_DETACH_FILTER, &detach_value,
                   sizeof(detach_value)) != -1 || errno != EPERM) {
        fail_message("cbpf-lock", "detach-errno");
    }
    close_checked(fd, "cbpf-lock-close");
    udp_pair_close(&udp);
    printf("THEKERNEL_PACKET_CBPF_BOUNDARY accept=1 drop=1 snaplen=8 "
           "replace=1 detach=1 envelope_boundary=1 invalid_jump_errno=%d "
           "locked_errno=%d\n",
           EINVAL, EPERM);
    fflush(stdout);
    marker("THEKERNEL_PACKET_CBPF_OK");
}

static void test_packet_options(void) {
    int fd = bound_packet_socket(SOCK_RAW, ETH_P_ALL, loopback_index);

    static const struct {
        int option;
        int expected;
    } introspection[] = {
        {SO_TYPE, SOCK_RAW},
        {SO_ERROR, 0},
        {SO_DOMAIN, AF_PACKET},
        {SO_PROTOCOL, 0},
    };
    for (size_t option_index = 0;
         option_index < sizeof(introspection) / sizeof(introspection[0]);
         ++option_index) {
        unsigned char expected_bytes[sizeof(int)];
        memcpy(expected_bytes, &introspection[option_index].expected,
               sizeof(expected_bytes));
        for (socklen_t requested = 0; requested <= sizeof(int); ++requested) {
            unsigned char output[sizeof(int) + 4];
            memset(output, 0xa5, sizeof(output));
            socklen_t output_length = requested;
            if (getsockopt(fd, SOL_SOCKET, introspection[option_index].option,
                           output, &output_length) != 0 ||
                output_length != requested ||
                memcmp(output, expected_bytes, requested) != 0) {
                fail_message("option-sol-socket", "short-introspection");
            }
            for (size_t tail = requested; tail < sizeof(output); ++tail) {
                require_true(output[tail] == 0xa5,
                             "option-sol-socket-short-tail");
            }
        }
        unsigned char output[sizeof(int) + 4];
        memset(output, 0xa5, sizeof(output));
        socklen_t output_length = sizeof(output);
        if (getsockopt(fd, SOL_SOCKET, introspection[option_index].option,
                       output, &output_length) != 0 ||
            output_length != sizeof(int) ||
            memcmp(output, expected_bytes, sizeof(expected_bytes)) != 0) {
            fail_message("option-sol-socket", "full-introspection");
        }
        for (size_t tail = sizeof(int); tail < sizeof(output); ++tail) {
            require_true(output[tail] == 0xa5,
                         "option-sol-socket-full-tail");
        }
    }
    printf("THEKERNEL_PACKET_SOL_SOCKET_BOUNDARY type=%d error=0 domain=%d "
           "protocol=0 short_copy=1\n",
           SOCK_RAW, AF_PACKET);
    fflush(stdout);

    int value = 0;
    socklen_t value_length = sizeof(value);
    if (getsockopt(fd, SOL_PACKET, PACKET_IGNORE_OUTGOING, &value,
                   &value_length) != 0 ||
        value_length != sizeof(value) || value != 0) {
        fail_message("option-ignore-outgoing", "initial-get");
    }
    value = 7;
    errno = 0;
    if (setsockopt(fd, SOL_PACKET, PACKET_IGNORE_OUTGOING, &value,
                   sizeof(value)) != -1 ||
        errno != EINVAL) {
        fail_message("option-ignore-outgoing", "invalid-boolean");
    }
    value = 1;
    if (setsockopt(fd, SOL_PACKET, PACKET_IGNORE_OUTGOING, &value,
                   sizeof(value)) != 0) {
        fail_message("option-ignore-outgoing", "set");
    }
    value = 0;
    value_length = sizeof(value);
    if (getsockopt(fd, SOL_PACKET, PACKET_IGNORE_OUTGOING, &value,
                   &value_length) != 0 ||
        value != 1) {
        fail_message("option-ignore-outgoing", "enabled-get");
    }
    unsigned char short_value[sizeof(int)];
    memset(short_value, 0xa5, sizeof(short_value));
    value_length = 1;
    if (getsockopt(fd, SOL_PACKET, PACKET_IGNORE_OUTGOING, short_value,
                   &value_length) != 0 ||
        value_length != 1 || short_value[0] != 1 || short_value[1] != 0xa5) {
        fail_message("option-ignore-outgoing", "short-get");
    }

    struct udp_pair udp;
    udp_pair_open(&udp);
    unsigned char token[16];
    make_token(token, 4, 1);
    udp_send_and_drain(&udp, token);
    struct packet_record record[1];
    collect_records(fd, token, sizeof(token), record, 1);
    require_packet_address(&record[0], ETH_P_IP, PACKET_HOST,
                           (const unsigned char[6]){0});
    require_no_matching_record(fd, token, sizeof(token));
    close_checked(fd, "option-ignore-outgoing-close");

    fd = bound_packet_socket(SOCK_DGRAM, ETH_P_IP, loopback_index);
    make_token(token, 4, 2);
    udp_send_and_drain(&udp, token);
    collect_records(fd, token, sizeof(token), record, 1);
    struct tpacket_stats statistics;
    memset(&statistics, 0, sizeof(statistics));
    socklen_t statistics_length = sizeof(statistics);
    if (getsockopt(fd, SOL_PACKET, PACKET_STATISTICS, &statistics,
                   &statistics_length) != 0) {
        fail_message("option-statistics", "first-get");
    }
    require_true(statistics_length == sizeof(statistics) &&
                     statistics.tp_packets == 1 && statistics.tp_drops == 0,
                 "option-statistics-first-values");
    memset(&statistics, 0xa5, sizeof(statistics));
    statistics_length = sizeof(statistics);
    if (getsockopt(fd, SOL_PACKET, PACKET_STATISTICS, &statistics,
                   &statistics_length) != 0) {
        fail_message("option-statistics", "second-get");
    }
    require_true(statistics.tp_packets == 0 && statistics.tp_drops == 0,
                 "option-statistics-reset");

    make_token(token, 4, 3);
    udp_send_and_drain(&udp, token);
    collect_records(fd, token, sizeof(token), record, 1);
    memset(&statistics, 0xa5, sizeof(statistics));
    statistics_length = 0;
    if (getsockopt(fd, SOL_PACKET, PACKET_STATISTICS, &statistics,
                   &statistics_length) != 0 ||
        statistics_length != 0) {
        fail_message("option-statistics", "zero-length-get");
    }
    require_true(((unsigned char *)&statistics)[0] == 0xa5,
                 "option-statistics-zero-length-no-copy");
    statistics_length = sizeof(statistics);
    if (getsockopt(fd, SOL_PACKET, PACKET_STATISTICS, &statistics,
                   &statistics_length) != 0 ||
        statistics.tp_packets != 0 || statistics.tp_drops != 0) {
        fail_message("option-statistics", "zero-length-reset");
    }

    make_token(token, 4, 4);
    udp_send_and_drain(&udp, token);
    collect_records(fd, token, sizeof(token), record, 1);
    memset(&statistics, 0xa5, sizeof(statistics));
    statistics_length = sizeof(uint32_t);
    if (getsockopt(fd, SOL_PACKET, PACKET_STATISTICS, &statistics,
                   &statistics_length) != 0 ||
        statistics_length != sizeof(uint32_t) || statistics.tp_packets != 1) {
        fail_message("option-statistics", "short-get");
    }
    statistics_length = sizeof(statistics);
    if (getsockopt(fd, SOL_PACKET, PACKET_STATISTICS, &statistics,
                   &statistics_length) != 0 ||
        statistics.tp_packets != 0 || statistics.tp_drops != 0) {
        fail_message("option-statistics", "short-get-reset");
    }

    void *inaccessible = mmap(NULL, 4096, PROT_NONE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (inaccessible == MAP_FAILED) {
        fail_message("option-statistics", "mmap");
    }
    make_token(token, 4, 5);
    udp_send_and_drain(&udp, token);
    collect_records(fd, token, sizeof(token), record, 1);
    statistics_length = sizeof(statistics);
    errno = 0;
    if (getsockopt(fd, SOL_PACKET, PACKET_STATISTICS, inaccessible,
                   &statistics_length) != -1 ||
        errno != EFAULT) {
        fail_message("option-statistics", "fault-get");
    }
    statistics_length = sizeof(statistics);
    if (getsockopt(fd, SOL_PACKET, PACKET_STATISTICS, &statistics,
                   &statistics_length) != 0 ||
        statistics.tp_packets != 0 || statistics.tp_drops != 0) {
        fail_message("option-statistics", "fault-get-reset");
    }
    if (munmap(inaccessible, 4096) != 0) {
        fail_message("option-statistics", "munmap");
    }

    value = 0;
    value_length = sizeof(value);
    int unsupported = getsockopt(fd, SOL_PACKET, PACKET_RX_RING, &value,
                                 &value_length);
    int unsupported_errno = errno;
    require_true(unsupported == -1 && unsupported_errno == ENOPROTOOPT,
                 "option-known-unsupported");
    value_length = sizeof(value);
    int unknown = getsockopt(fd, SOL_PACKET, 0x7fff, &value, &value_length);
    int unknown_errno = errno;
    require_true(unknown == -1 && unknown_errno == ENOPROTOOPT,
                 "option-unknown");
    printf("THEKERNEL_PACKET_OPTION_BOUNDARY known_get_errno=%d "
           "unknown_get_errno=%d stats_zero_short_fault_reset=1\n",
           unsupported_errno, unknown_errno);
    fflush(stdout);

    close_checked(fd, "option-statistics-close");
    test_classic_filter();
    udp_pair_close(&udp);
    marker("THEKERNEL_PACKET_OPTIONS_OK");
}

static void usage(const char *program) {
    fprintf(stderr,
            "Usage: %s [--linux-host] [--require-options]\n"
            "  --linux-host       enforce Linux capability/error ordering\n"
            "  --require-options  execute, never skip, the SOL_PACKET suite\n",
            program);
}

int main(int argc, char **argv) {
    const char *host_mode = getenv("THEKERNEL_PORTABLE_HOST");
    linux_host_mode = host_mode != NULL && host_mode[0] != '\0' &&
                      strcmp(host_mode, "0") != 0;
    require_options = linux_host_mode;
    for (int index = 1; index < argc; ++index) {
        if (strcmp(argv[index], "--linux-host") == 0) {
            linux_host_mode = true;
        } else if (strcmp(argv[index], "--require-options") == 0) {
            require_options = true;
        } else if (strcmp(argv[index], "--help") == 0) {
            usage(argv[0]);
            return EXIT_SUCCESS;
        } else {
            usage(argv[0]);
            return EXIT_FAILURE;
        }
    }

    loopback_index = if_nametoindex("lo");
    if (loopback_index == 0) {
        fail_message("loopback", "if-nametoindex");
    }

    if (linux_host_mode) {
        int probe = socket(AF_PACKET, SOCK_RAW | SOCK_CLOEXEC, htons(ETH_P_ALL));
        if (probe < 0 && (errno == EPERM || errno == EACCES)) {
            printf("THEKERNEL_PACKET_UNSUPPORTED raw_socket_errno=%d\n", errno);
            return 4;
        }
        if (probe < 0) {
            fail_message("raw-socket-probe", "socket");
        }
        close_checked(probe, "raw-socket-probe-close");
    }

    test_udp_receive_without_source_output();
    test_create();
    test_receive();
    test_fault_ownership();
    test_send();
    if (require_options) {
        test_packet_options();
    }
    marker("THEKERNEL_PACKET_OK");
    return EXIT_SUCCESS;
}
