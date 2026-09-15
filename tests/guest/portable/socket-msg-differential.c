/*
 * Socket message-transfer differential: send/recv flags, MSG_WAITALL
 * completion, and the recvmmsg batch deadline.
 *
 * Every record printed here must be identical on TheKernel and on Linux
 * v7.2.3, so each assertion is either an errno/value contract from
 * net/socket.c and net/ipv4/tcp.c or a payload check that both kernels are
 * required to satisfy.  Nothing depends on scheduling, addresses, or timing
 * values that differ between the two kernels.
 */
#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/netlink.h>
#include <netinet/in.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef MSG_WAITFORONE
#define MSG_WAITFORONE 0x10000
#endif
/* include/linux/socket.h:MSG_CMSG_COMPAT, nonzero because x86_64 builds with
 * CONFIG_COMPAT.  Unlike every other MSG_* bit, sendmsg/recvmsg/sendmmsg/
 * recvmmsg reject it with EINVAL before they resolve the descriptor. */
#define MSG_CMSG_COMPAT_BIT 0x80000000u

#define WAITALL_MILLISECONDS 400
#define BATCH_LENGTH 4
#define RECORD_CAPACITY 8

/* recvmmsg batches need real iovecs: a null msg_iov receives zero-length
 * records on both kernels. */
struct batch {
    struct mmsghdr entries[BATCH_LENGTH];
    struct iovec vectors[BATCH_LENGTH];
    char buffers[BATCH_LENGTH][RECORD_CAPACITY];
};

static void reset_batch(struct batch *batch) {
    memset(batch, 0, sizeof(*batch));
    for (unsigned index = 0; index < BATCH_LENGTH; index++) {
        batch->vectors[index].iov_base = batch->buffers[index];
        batch->vectors[index].iov_len = RECORD_CAPACITY;
        batch->entries[index].msg_hdr.msg_iov = &batch->vectors[index];
        batch->entries[index].msg_hdr.msg_iovlen = 1;
    }
}

static const char *active;

static void begin(const char *name) {
    active = name;
    printf("THEKERNEL_ABI_CASE %s\n", name);
}

static void mark(const char *name, int good) {
    printf("THEKERNEL_ABI_ASSERT %s %s %s\n", active, name, good ? "pass" : "fail");
    if (!good) {
        fprintf(stderr, "%s: %s: errno=%d (%s)\n", active, name, errno, strerror(errno));
        exit(1);
    }
}

static void done(void) {
    printf("THEKERNEL_ABI_RESULT %s pass\n", active);
}

static void on_alarm(int signal_number) {
    (void)signal_number;
    static const char message[] = "THEKERNEL_SOCKET_MSG_FAIL alarm\n";
    ssize_t ignored = write(STDERR_FILENO, message, sizeof(message) - 1);
    (void)ignored;
    _exit(1);
}

static void sleep_milliseconds(long milliseconds) {
    struct timespec delay = {
        .tv_sec = milliseconds / 1000,
        .tv_nsec = (milliseconds % 1000) * 1000000L,
    };
    while (nanosleep(&delay, &delay) != 0 && errno == EINTR) {
    }
}

static int write_all(int fd, const void *buffer, size_t length) {
    const unsigned char *bytes = buffer;
    size_t written = 0;
    while (written < length) {
        ssize_t count = write(fd, bytes + written, length - written);
        if (count <= 0) {
            return -1;
        }
        written += (size_t)count;
    }
    return 0;
}

static int read_exactly(int fd, void *buffer, size_t length) {
    unsigned char *bytes = buffer;
    size_t read_bytes = 0;
    while (read_bytes < length) {
        ssize_t count = read(fd, bytes + read_bytes, length - read_bytes);
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count <= 0) {
            return -1;
        }
        read_bytes += (size_t)count;
    }
    return 0;
}

static void set_receive_timeout(int fd, long seconds) {
    struct timeval timeout = {.tv_sec = seconds, .tv_usec = 0};
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0) {
        exit(1);
    }
}

/* Binds a loopback UDP receiver and returns a sender connected to it. */
static void udp_pair(int *receiver, int *sender) {
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t length = sizeof(address);
    *receiver = socket(AF_INET, SOCK_DGRAM, 0);
    *sender = socket(AF_INET, SOCK_DGRAM, 0);
    if (*receiver < 0 || *sender < 0) {
        exit(1);
    }
    if (bind(*receiver, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        getsockname(*receiver, (struct sockaddr *)&address, &length) != 0 ||
        connect(*sender, (struct sockaddr *)&address, sizeof(address)) != 0) {
        exit(1);
    }
    set_receive_timeout(*receiver, 5);
}

/* Helpers run as threads, not forked children.  A forked child shares the
 * serial console, and TheKernel's console re-emits a line from a child that
 * exits while the parent is blocked on it, which would put a second copy of an
 * assertion record into the differential transcript.  Threads keep every
 * record printed exactly once by one process. */
struct helper {
    /* Accepted endpoint, when this helper owns a loopback TCP connection. */
    int listener;
    /* Stream endpoint, when this helper owns a socketpair direction. */
    int fd;
    const char *first;
    size_t first_length;
    const char *second;
    size_t second_length;
    long delay_milliseconds;
    /* Written once by the helper; read only after `pthread_join`. */
    int verdict;
};

static void write_bursts(struct helper *helper, int fd) {
    helper->verdict = 0;
    if (write_all(fd, helper->first, helper->first_length) != 0) {
        return;
    }
    if (helper->second_length != 0) {
        sleep_milliseconds(helper->delay_milliseconds);
        if (write_all(fd, helper->second, helper->second_length) != 0) {
            return;
        }
    }
    helper->verdict = 1;
}

static void *stream_writer(void *argument) {
    struct helper *helper = argument;
    write_bursts(helper, helper->fd);
    close(helper->fd);
    return NULL;
}

static void *tcp_writer(void *argument) {
    struct helper *helper = argument;
    int peer = accept(helper->listener, NULL, NULL);
    close(helper->listener);
    if (peer >= 0) {
        write_bursts(helper, peer);
        close(peer);
    }
    return NULL;
}

/* Reads the three bytes the MSG_MORE case sends and then requires end-of-file,
 * so a cork that never released the first two bytes cannot pass. */
static void *tcp_verifier(void *argument) {
    struct helper *helper = argument;
    int peer = accept(helper->listener, NULL, NULL);
    close(helper->listener);
    char payload[4] = {0};
    helper->verdict = peer >= 0 && read_exactly(peer, payload, 3) == 0 &&
                      memcmp(payload, "xxy", 3) == 0 && recv(peer, payload, 1, 0) == 0;
    if (peer >= 0) {
        close(peer);
    }
    return NULL;
}

/* sendto/sendmsg accept the protocol flags Linux never rejects at the socket
 * layer.  net/ipv4/udp.c:udp_sendmsg() consumes MSG_CONFIRM and
 * ip_sendmsg_scope() consumes MSG_DONTROUTE; neither can fail a datagram that
 * a loopback route already carries.  MSG_MORE is deliberately absent here:
 * udp_sendmsg() corks those payloads into one later datagram, which this
 * differential cannot state without depending on transmit timing. */
static void case_send_flags(void) {
    begin("socket_msg.send_flags.raw-differential");
    int receiver = -1;
    int sender = -1;
    udp_pair(&receiver, &sender);

    errno = 0;
    mark("DONTROUTE_SENDTO", sendto(sender, "a", 1, MSG_DONTROUTE, NULL, 0) == 1);
    errno = 0;
    mark("CONFIRM_SENDTO", sendto(sender, "b", 1, MSG_CONFIRM, NULL, 0) == 1);
    errno = 0;
    mark("COMBINED_SENDTO",
         sendto(sender, "c", 1, MSG_DONTROUTE | MSG_CONFIRM | MSG_DONTWAIT, NULL, 0) == 1);

    struct iovec iov = {.iov_base = (void *)"d", .iov_len = 1};
    struct msghdr header = {.msg_iov = &iov, .msg_iovlen = 1};
    errno = 0;
    mark("CONFIRM_SENDMSG", sendmsg(sender, &header, MSG_CONFIRM) == 1);
    iov.iov_base = (void *)"e";
    errno = 0;
    mark("DONTROUTE_SENDMSG", sendmsg(sender, &header, MSG_DONTROUTE | MSG_DONTWAIT) == 1);

    /* `MSG_EOR` is a stream-position marker the datagram path ignores, an
     * undefined bit (0x200000 is not allocated in include/linux/socket.h) is
     * carried past the socket layer, and `MSG_ZEROCOPY` without `SO_ZEROCOPY`
     * falls back to an ordinary copy: zero-copy completion is only armed when
     * the socket option is set (`sk_zerocopy`, `net/core/sock.c:1450-1456`),
     * so none of the three can fail a UDP datagram. */
    errno = 0;
    mark("EOR_SENDTO", sendto(sender, "f", 1, MSG_EOR, NULL, 0) == 1);
    errno = 0;
    mark("ZEROCOPY_SENDTO", sendto(sender, "g", 1, MSG_ZEROCOPY, NULL, 0) == 1);
    iov.iov_base = (void *)"h";
    errno = 0;
    mark("UNDEFINED_FLAG_SENDMSG", sendmsg(sender, &header, 0x200000) == 1);

    char payload[8] = {0};
    int complete = 1;
    for (int index = 0; index < 8; index++) {
        ssize_t count = recv(receiver, &payload[index], 1, 0);
        if (count != 1) {
            complete = 0;
            break;
        }
    }
    mark("FLAGGED_DATAGRAMS_DELIVERED", complete && memcmp(payload, "abcdefgh", 8) == 0);

    close(receiver);
    close(sender);

    /* MSG_OOB is not a socket-layer refusal: `__sys_sendto()` clears only
     * MSG_INTERNAL_SENDMSG_FLAGS and forwards the rest
     * (`net/socket.c:2248-2252`), so each protocol answers for itself.
     * `tcp_sendmsg_locked()` consumes the bit through `tcp_mark_urg()` and
     * still sends the octets (`net/ipv4/tcp.c:715-718`), `udp_sendmsg()`
     * mirrors the BSD EOPNOTSUPP (`net/ipv4/udp.c:1259-1261`), and an AF_UNIX
     * stream built with CONFIG_AF_UNIX_OOB reserves the last octet for
     * `queue_oob()` and counts it as sent (`net/unix/af_unix.c:2392-2400`,
     * `:2496-2502`). */
    struct sockaddr_in stream_address = {.sin_family = AF_INET,
                                         .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t stream_length = sizeof(stream_address);
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    int connected = -1;
    int stream_ready =
        listener >= 0 && bind(listener, (struct sockaddr *)&stream_address, sizeof(stream_address)) == 0 &&
        listen(listener, 1) == 0 &&
        getsockname(listener, (struct sockaddr *)&stream_address, &stream_length) == 0 &&
        (connected = socket(AF_INET, SOCK_STREAM, 0)) >= 0 &&
        connect(connected, (struct sockaddr *)&stream_address, stream_length) == 0;
    errno = 0;
    mark("OOB_SENDTO_STREAM_BYTES", stream_ready && send(connected, "i", 1, MSG_OOB) == 1);
    struct iovec oob_vector = {.iov_base = (void *)"j", .iov_len = 1};
    struct msghdr oob_header = {.msg_iov = &oob_vector, .msg_iovlen = 1};
    errno = 0;
    mark("OOB_SENDMSG_STREAM_BYTES", stream_ready && sendmsg(connected, &oob_header, MSG_OOB) == 1);

    int datagram = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in datagram_address = {.sin_family = AF_INET,
                                           .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t datagram_length = sizeof(datagram_address);
    errno = 0;
    mark("OOB_SENDTO_DATAGRAM_EOPNOTSUPP",
         datagram >= 0 &&
             bind(datagram, (struct sockaddr *)&datagram_address, sizeof(datagram_address)) == 0 &&
             getsockname(datagram, (struct sockaddr *)&datagram_address, &datagram_length) == 0 &&
             sendto(datagram, "k", 1, MSG_OOB, (struct sockaddr *)&datagram_address,
                    sizeof(datagram_address)) == -1 &&
             errno == EOPNOTSUPP);

    int unix_pair[2] = {-1, -1};
    int unix_ready = socketpair(AF_UNIX, SOCK_STREAM, 0, unix_pair) == 0;
    errno = 0;
    mark("OOB_SENDTO_UNIX_STREAM_BYTES", unix_ready && send(unix_pair[0], "l", 1, MSG_OOB) == 1);

    /* The receive half.  `tcp_recvmsg_locked()` diverts the bit to `recv_urg`
     * before anything else (`net/ipv4/tcp.c:2679-2681`) and `tcp_recv_urg()`
     * answers -EINVAL while this endpoint has no urgent byte pending
     * (`:1480-1483`); `udp_recvmsg()` never reads the bit, so the datagram is
     * delivered normally; `unix_dgram_recvmsg()` refuses it outright
     * (`net/unix/af_unix.c:2572-2576`); an AF_UNIX stream under
     * CONFIG_AF_UNIX_OOB answers -EINVAL through `unix_stream_recv_urg()`
     * while its `oob_skb` is NULL (`net/unix/af_unix.c:2929-2934`,
     * `:2771-2776`). */
    char oob_buffer[8] = {0};
    errno = 0;
    mark("OOB_RECV_STREAM_EINVAL",
         stream_ready && recv(connected, oob_buffer, sizeof(oob_buffer), MSG_OOB) == -1 &&
             errno == EINVAL);
    struct iovec recv_vector = {.iov_base = oob_buffer, .iov_len = sizeof(oob_buffer)};
    struct msghdr recv_header = {.msg_iov = &recv_vector, .msg_iovlen = 1};
    errno = 0;
    mark("OOB_RECVMSG_STREAM_EINVAL",
         stream_ready && recvmsg(connected, &recv_header, MSG_OOB) == -1 && errno == EINVAL);
    errno = 0;
    mark("OOB_RECV_DATAGRAM_IGNORED",
         sendto(datagram, "m", 1, 0, (struct sockaddr *)&datagram_address,
                sizeof(datagram_address)) == 1 &&
             recv(datagram, oob_buffer, sizeof(oob_buffer), MSG_OOB) == 1 && oob_buffer[0] == 'm');
    int unix_datagram_pair[2] = {-1, -1};
    int unix_datagram_ready =
        socketpair(AF_UNIX, SOCK_DGRAM, 0, unix_datagram_pair) == 0;
    errno = 0;
    mark("OOB_RECV_UNIX_DATAGRAM_EOPNOTSUPP",
         unix_datagram_ready &&
             recv(unix_datagram_pair[1], oob_buffer, sizeof(oob_buffer), MSG_OOB | MSG_DONTWAIT) ==
                 -1 &&
             errno == EOPNOTSUPP);
    /* A second stream pair, because the first one's peer already holds the
     * octet `queue_oob()` reserved: an urgent byte *is* pending there, and a
     * receive of it is a different Linux answer. */
    int quiet_pair[2] = {-1, -1};
    int quiet_ready = socketpair(AF_UNIX, SOCK_STREAM, 0, quiet_pair) == 0;
    errno = 0;
    mark("OOB_RECV_UNIX_STREAM_EINVAL",
         quiet_ready &&
             recv(quiet_pair[1], oob_buffer, sizeof(oob_buffer), MSG_OOB | MSG_DONTWAIT) == -1 &&
             errno == EINVAL);

    /* `netlink_recvmsg()` tests MSG_OOB before it looks at the receive queue
     * at all (`net/netlink/af_netlink.c:1915-1917`). */
    int netlink = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    errno = 0;
    mark("OOB_RECV_NETLINK_EOPNOTSUPP",
         netlink >= 0 &&
             recv(netlink, oob_buffer, sizeof(oob_buffer), MSG_OOB | MSG_DONTWAIT) == -1 &&
             errno == EOPNOTSUPP);

    if (netlink >= 0) {
        close(netlink);
    }
    if (quiet_ready) {
        close(quiet_pair[0]);
        close(quiet_pair[1]);
    }
    if (unix_datagram_ready) {
        close(unix_datagram_pair[0]);
        close(unix_datagram_pair[1]);
    }
    if (unix_ready) {
        close(unix_pair[0]);
        close(unix_pair[1]);
    }
    if (datagram >= 0) {
        close(datagram);
    }
    if (connected >= 0) {
        close(connected);
    }
    if (listener >= 0) {
        close(listener);
    }
    done();
}

/* `__sys_sendmmsg` runs the same send path as sendmsg with its own `flags`
 * argument for every element: it ORs in `MSG_BATCH` for all but the last
 * message, and `____sys_sendmsg` then replaces `msg_sys->msg_flags` with those
 * call flags, keeping only a per-message `MSG_EOR` because that bit is in
 * `allowed_msghdr_flags` (`net/socket.c:2782-2836`, `:2628-2678`).  A
 * per-message `MSG_MORE` is therefore discarded, while `MSG_MORE` as the call
 * flag corks the whole batch into one datagram that the next plain send
 * flushes (`net/ipv4/udp.c:udp_sendmsg()`). */
static void case_sendmmsg_flags(void) {
    begin("socket_msg.sendmmsg_flags.raw-differential");
    int receiver = -1;
    int sender = -1;
    udp_pair(&receiver, &sender);

    char first[4] = "ab";
    char second[4] = "cde";
    struct iovec vectors[2] = {{first, 2}, {second, 3}};
    struct mmsghdr batch[2];
    memset(batch, 0, sizeof(batch));
    for (int index = 0; index < 2; index++) {
        batch[index].msg_hdr.msg_iov = &vectors[index];
        batch[index].msg_hdr.msg_iovlen = 1;
    }

    errno = 0;
    int sent = sendmmsg(sender, batch, 2, 0);
    mark("SENDMMSG_TWO", sent == 2 && batch[0].msg_len == 2 && batch[1].msg_len == 3);
    char record[8] = {0};
    mark("SENDMMSG_TWO_RECORDS",
         recv(receiver, record, sizeof(record), 0) == 2 && memcmp(record, "ab", 2) == 0 &&
             recv(receiver, record, sizeof(record), 0) == 3 && memcmp(record, "cde", 3) == 0);

    errno = 0;
    mark("SENDMMSG_EOR", sendmmsg(sender, batch, 1, MSG_EOR) == 1);
    mark("SENDMMSG_EOR_RECORD", recv(receiver, record, sizeof(record), 0) == 2);
    errno = 0;
    mark("SENDMMSG_UNDEFINED_FLAG", sendmmsg(sender, batch, 1, 0x200000) == 1);
    mark("SENDMMSG_UNDEFINED_RECORD", recv(receiver, record, sizeof(record), 0) == 2);

    /* A per-message MSG_MORE is not the call flag, so both messages leave as
     * their own datagram. */
    batch[0].msg_hdr.msg_flags = MSG_MORE;
    batch[1].msg_hdr.msg_flags = MSG_MORE;
    errno = 0;
    sent = sendmmsg(sender, batch, 2, 0);
    mark("SENDMMSG_HEADER_FLAGS_IGNORED",
         sent == 2 && recv(receiver, record, sizeof(record), 0) == 2 &&
             recv(receiver, record, sizeof(record), 0) == 3);
    batch[0].msg_hdr.msg_flags = 0;
    batch[1].msg_hdr.msg_flags = 0;

    /* The call flag corks the batch: nothing is readable until a send without
     * MSG_MORE flushes the pending payload as one datagram. */
    errno = 0;
    sent = sendmmsg(sender, batch, 2, MSG_MORE);
    mark("SENDMMSG_MORE_CORKS", sent == 2 && batch[0].msg_len == 2 && batch[1].msg_len == 3);
    errno = 0;
    mark("SENDMMSG_MORE_PENDING",
         recv(receiver, record, sizeof(record), MSG_DONTWAIT) == -1 && errno == EAGAIN);
    errno = 0;
    mark("SENDMMSG_MORE_FLUSH", send(sender, "z", 1, 0) == 1);
    memset(record, 0, sizeof(record));
    mark("SENDMMSG_MORE_MERGED",
         recv(receiver, record, sizeof(record), 0) == 6 && memcmp(record, "abcdez", 6) == 0);

    close(receiver);
    close(sender);

    /* A batch reaches the same protocol entry point per message, so the
     * per-transport MSG_OOB answer is unchanged: `tcp_sendmsg_locked()`
     * consumes the bit and still sends the octet (`net/ipv4/tcp.c:715-718`),
     * and `____sys_sendmsg()` only substitutes the call flags for the
     * per-message word (`net/socket.c:2628-2678`). */
    struct sockaddr_in stream_address = {.sin_family = AF_INET,
                                         .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t stream_length = sizeof(stream_address);
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    int connected = -1;
    int stream_ready =
        listener >= 0 && bind(listener, (struct sockaddr *)&stream_address, sizeof(stream_address)) == 0 &&
        listen(listener, 1) == 0 &&
        getsockname(listener, (struct sockaddr *)&stream_address, &stream_length) == 0 &&
        (connected = socket(AF_INET, SOCK_STREAM, 0)) >= 0 &&
        connect(connected, (struct sockaddr *)&stream_address, stream_length) == 0;
    char urgent = 'n';
    struct iovec urgent_vector = {.iov_base = &urgent, .iov_len = 1};
    struct mmsghdr urgent_batch;
    memset(&urgent_batch, 0, sizeof(urgent_batch));
    urgent_batch.msg_hdr.msg_iov = &urgent_vector;
    urgent_batch.msg_hdr.msg_iovlen = 1;
    errno = 0;
    mark("OOB_SENDMMSG_STREAM_BYTES",
         stream_ready && sendmmsg(connected, &urgent_batch, 1, MSG_OOB) == 1 &&
             urgent_batch.msg_len == 1);
    if (connected >= 0) {
        close(connected);
    }
    if (listener >= 0) {
        close(listener);
    }
    done();
}

/* `tcp_recvmsg_locked()` counts the octets a peek copied towards
 * `sock_rcvlowat(sk, flags & MSG_WAITALL, len)` and advances its `peek_seq`
 * cursor over them (`net/ipv4/tcp.c:2701-2702`, `:2874-2877`), so a
 * `MSG_WAITALL|MSG_PEEK` receive blocks until the whole request is readable,
 * returns it, and consumes nothing: the receive that follows still sees all
 * ten octets.  An AF_UNIX stream is deliberately not asserted here, because
 * `unix_stream_read_generic()` leaves its copy loop on a peek that runs out of
 * queue (`net/unix/af_unix.c:3068-3070`) and returns the available prefix. */
static void case_peek_waitall_tcp(void) {
    begin("socket_msg.peek_waitall_tcp.raw-differential");
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t length = sizeof(address);
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    mark("TCP_PEEK_LISTENER",
         listener >= 0 && bind(listener, (struct sockaddr *)&address, sizeof(address)) == 0 &&
             listen(listener, 1) == 0 &&
             getsockname(listener, (struct sockaddr *)&address, &length) == 0);

    struct helper helper = {.listener = listener, .first = "abcd", .first_length = 4,
                            .second = "efghij", .second_length = 6,
                            .delay_milliseconds = WAITALL_MILLISECONDS};
    pthread_t writer;
    mark("TCP_PEEK_HELPER", pthread_create(&writer, NULL, tcp_writer, &helper) == 0);
    int client = socket(AF_INET, SOCK_STREAM, 0);
    mark("TCP_PEEK_CONNECT",
         client >= 0 && connect(client, (struct sockaddr *)&address, length) == 0);
    set_receive_timeout(client, 5);

    char payload[16] = {0};
    errno = 0;
    ssize_t count = recv(client, payload, 10, MSG_WAITALL | MSG_PEEK);
    mark("TCP_PEEK_TEN", count == 10 && memcmp(payload, "abcdefghij", 10) == 0);
    memset(payload, 0, sizeof(payload));
    errno = 0;
    count = recv(client, payload, 10, MSG_WAITALL);
    mark("TCP_PEEK_LEAVES_QUEUE", count == 10 && memcmp(payload, "abcdefghij", 10) == 0);

    close(client);
    mark("TCP_PEEK_JOIN", pthread_join(writer, NULL) == 0);
    mark("TCP_PEEK_DONE", helper.verdict == 1);
    done();
}

/* MSG_MORE is the per-call TCP_CORK: tcp_sendmsg_locked() pushes a partial
 * segment only for a send without MSG_MORE, and tcp_push() then releases
 * everything gathered since.  A reader thread verifies that all three bytes
 * arrive, in order, once the uncorking send runs; that nothing was delivered
 * before then is not asserted, because that timing is not part of the
 * contract. */
static void case_tcp_more(void) {
    begin("socket_msg.tcp_more.raw-differential");
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t length = sizeof(address);
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    mark("TCP_MORE_LISTENER",
         listener >= 0 && bind(listener, (struct sockaddr *)&address, sizeof(address)) == 0 &&
             listen(listener, 1) == 0 &&
             getsockname(listener, (struct sockaddr *)&address, &length) == 0);

    struct helper helper = {.listener = listener};
    pthread_t reader;
    mark("TCP_MORE_HELPER", pthread_create(&reader, NULL, tcp_verifier, &helper) == 0);
    int client = socket(AF_INET, SOCK_STREAM, 0);
    mark("TCP_MORE_CONNECT",
         client >= 0 && connect(client, (struct sockaddr *)&address, length) == 0);

    struct iovec iov = {.iov_base = (void *)"x", .iov_len = 1};
    struct msghdr header = {.msg_iov = &iov, .msg_iovlen = 1};
    errno = 0;
    mark("MORE_SEND", send(client, "x", 1, MSG_MORE) == 1);
    errno = 0;
    mark("MORE_SENDMSG", sendmsg(client, &header, MSG_MORE) == 1);
    errno = 0;
    mark("UNCORK_SEND", send(client, "y", 1, 0) == 1);
    close(client);

    mark("TCP_MORE_JOIN", pthread_join(reader, NULL) == 0);
    mark("MORE_STREAM_DELIVERED", helper.verdict == 1);
    done();
}

/* sendmsg/recvmsg/sendmmsg/recvmmsg reject MSG_CMSG_COMPAT before the
 * descriptor is resolved, so a bad descriptor reports EINVAL rather than
 * EBADF.  sendto/recvfrom never perform that check. */
static void case_compat_flag(void) {
    begin("socket_msg.compat_flag.raw-differential");
    struct iovec iov = {.iov_base = (void *)"x", .iov_len = 1};
    struct msghdr header = {.msg_iov = &iov, .msg_iovlen = 1};
    struct mmsghdr vector = {0};
    char byte = 0;

    errno = 0;
    mark("SENDMSG_COMPAT_EINVAL",
         syscall(SYS_sendmsg, -1, &header, MSG_CMSG_COMPAT_BIT) == -1 && errno == EINVAL);
    errno = 0;
    mark("SENDMSG_COMPAT_MORE_EINVAL",
         syscall(SYS_sendmsg, -1, &header, MSG_CMSG_COMPAT_BIT | MSG_MORE) == -1 && errno == EINVAL);
    errno = 0;
    mark("RECVMSG_COMPAT_EINVAL",
         syscall(SYS_recvmsg, -1, &header, MSG_CMSG_COMPAT_BIT) == -1 && errno == EINVAL);
    errno = 0;
    mark("SENDMMSG_COMPAT_EINVAL",
         syscall(SYS_sendmmsg, -1, &vector, 1, MSG_CMSG_COMPAT_BIT) == -1 && errno == EINVAL);
    errno = 0;
    mark("RECVMMSG_COMPAT_EINVAL",
         syscall(SYS_recvmmsg, -1, &vector, 1, MSG_CMSG_COMPAT_BIT, NULL) == -1 && errno == EINVAL);

    errno = 0;
    mark("SENDMSG_BADF", syscall(SYS_sendmsg, -1, &header, 0) == -1 && errno == EBADF);
    errno = 0;
    mark("SENDMMSG_BADF", syscall(SYS_sendmmsg, -1, &vector, 1, 0) == -1 && errno == EBADF);
    errno = 0;
    mark("RECVMMSG_BADF", syscall(SYS_recvmmsg, -1, &vector, 1, 0, NULL) == -1 && errno == EBADF);
    errno = 0;
    mark("SENDTO_COMPAT_ACCEPTED",
         syscall(SYS_sendto, -1, &byte, 1, MSG_CMSG_COMPAT_BIT, NULL, 0) == -1 && errno == EBADF);
    done();
}

/* MSG_WAITALL over a byte stream: a short read continues until the whole
 * request, end-of-file, a signal, or a non-blocking exit.  The writer's second
 * burst is delayed so the first receive attempt is guaranteed to be short. */
static void case_waitall_stream(void) {
    begin("socket_msg.waitall_stream.raw-differential");
    char payload[16] = {0};

    int pair[2];
    mark("STREAM_MERGE_PAIR", socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
    struct helper helper = {.fd = pair[1], .first = "abcd", .first_length = 4,
                            .second = "efghij", .second_length = 6,
                            .delay_milliseconds = WAITALL_MILLISECONDS};
    pthread_t writer;
    mark("STREAM_MERGE_WRITER", pthread_create(&writer, NULL, stream_writer, &helper) == 0);
    errno = 0;
    ssize_t count = recv(pair[0], payload, 10, MSG_WAITALL);
    mark("STREAM_MERGE_TEN", count == 10 && memcmp(payload, "abcdefghij", 10) == 0);
    /* Join before reading the helper's verdict: `pthread_join` is what makes
     * the write in `write_bursts` visible, and reading it earlier would be a
     * data race the compiler is free to exploit. */
    int joined = pthread_join(writer, NULL);
    mark("STREAM_MERGE_DONE", joined == 0 && helper.verdict == 1);
    close(pair[0]);

    mark("STREAM_EOF_PAIR", socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
    struct helper eof = {.fd = pair[1], .first = "xyz", .first_length = 3};
    mark("STREAM_EOF_WRITER", pthread_create(&writer, NULL, stream_writer, &eof) == 0);
    errno = 0;
    count = recv(pair[0], payload, 100, MSG_WAITALL);
    mark("STREAM_EOF_SHORT", count == 3 && memcmp(payload, "xyz", 3) == 0);
    errno = 0;
    mark("STREAM_EOF_ZERO", recv(pair[0], payload, 100, MSG_WAITALL) == 0);
    joined = pthread_join(writer, NULL);
    mark("STREAM_EOF_DONE", joined == 0 && eof.verdict == 1);
    close(pair[0]);

    mark("STREAM_EMPTY_PAIR", socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
    errno = 0;
    mark("STREAM_EMPTY_EAGAIN",
         recv(pair[0], payload, 100, MSG_WAITALL | MSG_DONTWAIT) == -1 && errno == EAGAIN);
    close(pair[0]);
    close(pair[1]);
    done();
}

/* The same completion rule over TCP, where sock_rcvlowat() is applied by
 * tcp_recvmsg_locked(). */
static void case_waitall_tcp(void) {
    begin("socket_msg.waitall_tcp.raw-differential");
    struct sockaddr_in address = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t length = sizeof(address);
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    mark("TCP_WAITALL_LISTENER",
         listener >= 0 && bind(listener, (struct sockaddr *)&address, sizeof(address)) == 0 &&
             listen(listener, 1) == 0 &&
             getsockname(listener, (struct sockaddr *)&address, &length) == 0);

    struct helper helper = {.listener = listener, .first = "abcd", .first_length = 4,
                            .second = "efghij", .second_length = 6,
                            .delay_milliseconds = WAITALL_MILLISECONDS};
    pthread_t writer;
    mark("TCP_WAITALL_HELPER", pthread_create(&writer, NULL, tcp_writer, &helper) == 0);
    int client = socket(AF_INET, SOCK_STREAM, 0);
    mark("TCP_WAITALL_CONNECT",
         client >= 0 && connect(client, (struct sockaddr *)&address, length) == 0);
    set_receive_timeout(client, 5);

    char payload[16] = {0};
    errno = 0;
    ssize_t count = recv(client, payload, 10, MSG_WAITALL);
    mark("TCP_MERGE_TEN", count == 10 && memcmp(payload, "abcdefghij", 10) == 0);
    errno = 0;
    mark("TCP_EOF_ZERO", recv(client, payload, 10, MSG_WAITALL) == 0);
    close(client);
    int joined = pthread_join(writer, NULL);
    mark("TCP_WAITALL_DONE", joined == 0 && helper.verdict == 1);
    done();
}

/* A datagram protocol never consults MSG_WAITALL, so one record is returned
 * even when the request is larger. */
static void case_waitall_datagram(void) {
    begin("socket_msg.waitall_datagram.raw-differential");
    int receiver = -1;
    int sender = -1;
    udp_pair(&receiver, &sender);

    errno = 0;
    mark("DATAGRAM_SENT", send(sender, "wxyz", 4, 0) == 4);
    char payload[16] = {0};
    errno = 0;
    ssize_t count = recv(receiver, payload, 100, MSG_WAITALL);
    mark("DATAGRAM_SINGLE_RECORD", count == 4 && memcmp(payload, "wxyz", 4) == 0);

    errno = 0;
    mark("DATAGRAM_SENT_AGAIN", send(sender, "wxyz", 4, 0) == 4);
    errno = 0;
    count = recv(receiver, payload, 100, MSG_WAITALL | MSG_PEEK);
    mark("DATAGRAM_PEEK_RECORD", count == 4 && memcmp(payload, "wxyz", 4) == 0);

    close(receiver);
    close(sender);
    done();
}

/* do_recvmmsg() turns the relative timeout into one absolute deadline and
 * rewrites the user timespec only after a datagram was received.  A batch that
 * receives nothing leaves the timespec untouched; an expired deadline stops
 * the batch with the remaining interval stored. */
static void case_recvmmsg_deadline(void) {
    begin("socket_msg.recvmmsg_deadline.raw-differential");
    int receiver = -1;
    int sender = -1;
    udp_pair(&receiver, &sender);
    if (fcntl(receiver, F_SETFL, O_NONBLOCK) != 0) {
        exit(1);
    }

    struct batch batch;
    struct timespec timeout = {.tv_sec = 0, .tv_nsec = 500000000L};
    reset_batch(&batch);
    errno = 0;
    int count = recvmmsg(receiver, batch.entries, BATCH_LENGTH, MSG_DONTWAIT, &timeout);
    mark("EMPTY_EAGAIN", count == -1 && errno == EAGAIN);
    mark("EMPTY_TIMEOUT_UNCHANGED", timeout.tv_sec == 0 && timeout.tv_nsec == 500000000L);

    errno = 0;
    mark("QUEUE_TWO", send(sender, "12", 2, 0) == 2 && send(sender, "34", 2, 0) == 2);
    timeout.tv_sec = 5;
    timeout.tv_nsec = 0;
    reset_batch(&batch);
    errno = 0;
    count = recvmmsg(receiver, batch.entries, BATCH_LENGTH, MSG_DONTWAIT, &timeout);
    mark("PARTIAL_BATCH_COUNT", count == 2);
    mark("PARTIAL_BATCH_PAYLOADS",
         batch.entries[0].msg_len == 2 && batch.entries[1].msg_len == 2 &&
         memcmp(batch.buffers[0], "12", 2) == 0 && memcmp(batch.buffers[1], "34", 2) == 0);
    mark("PARTIAL_BATCH_REMAINING",
         timeout.tv_sec >= 0 && timeout.tv_sec <= 5 && timeout.tv_nsec >= 0 &&
         timeout.tv_nsec < 1000000000L && (timeout.tv_sec != 0 || timeout.tv_nsec != 0));

    /* poll_select_set_timeout() keeps a zero timeout at zero, so the deadline
     * is already expired after the first datagram. */
    errno = 0;
    mark("QUEUE_TWO_AGAIN", send(sender, "56", 2, 0) == 2 && send(sender, "78", 2, 0) == 2);
    timeout.tv_sec = 0;
    timeout.tv_nsec = 0;
    reset_batch(&batch);
    errno = 0;
    count = recvmmsg(receiver, batch.entries, BATCH_LENGTH, MSG_DONTWAIT, &timeout);
    mark("ZERO_TIMEOUT_ONE_DATAGRAM", count == 1 && batch.entries[0].msg_len == 2);
    mark("ZERO_TIMEOUT_STORED", timeout.tv_sec == 0 && timeout.tv_nsec == 0);

    /* A non-normalised timeout is rejected before the descriptor is resolved. */
    errno = 0;
    mark("QUEUE_INVALID", send(sender, "9", 1, 0) == 1);
    struct timespec invalid = {.tv_sec = 0, .tv_nsec = 1000000000L};
    reset_batch(&batch);
    errno = 0;
    count = recvmmsg(receiver, batch.entries, 1, MSG_DONTWAIT, &invalid);
    mark("NONNORMALIZED_TIMEOUT_EINVAL", count == -1 && errno == EINVAL);
    errno = 0;
    count = syscall(SYS_recvmmsg, -1, batch.entries, 1, 0, &invalid);
    mark("TIMEOUT_BEFORE_BADF", count == -1 && errno == EINVAL);

    /* A zero-length batch neither receives nor rewrites the timespec. */
    struct timespec untouched = {.tv_sec = 7, .tv_nsec = 250000000L};
    errno = 0;
    count = recvmmsg(receiver, batch.entries, 0, MSG_DONTWAIT, &untouched);
    mark("ZERO_VLEN_NO_BATCH", count == 0);
    mark("ZERO_VLEN_TIMEOUT_UNCHANGED", untouched.tv_sec == 7 && untouched.tv_nsec == 250000000L);

    char drain[4];
    while (recv(receiver, drain, sizeof(drain), 0) > 0) {
    }
    close(receiver);
    close(sender);

    /* `do_recvmmsg()` hands the call flags to every message, so the batch
     * primitive reports the same per-transport MSG_OOB answer as `recvmsg`:
     * `tcp_recvmsg_locked()` diverts the bit to `recv_urg`
     * (`net/ipv4/tcp.c:2679-2681`) and `tcp_recv_urg()` answers -EINVAL while
     * no urgent byte is pending on this endpoint (`:1480-1483`). */
    struct sockaddr_in stream_address = {.sin_family = AF_INET,
                                         .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t stream_length = sizeof(stream_address);
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    int connected = -1;
    int stream_ready =
        listener >= 0 && bind(listener, (struct sockaddr *)&stream_address, sizeof(stream_address)) == 0 &&
        listen(listener, 1) == 0 &&
        getsockname(listener, (struct sockaddr *)&stream_address, &stream_length) == 0 &&
        (connected = socket(AF_INET, SOCK_STREAM, 0)) >= 0 &&
        connect(connected, (struct sockaddr *)&stream_address, stream_length) == 0;
    reset_batch(&batch);
    errno = 0;
    count = recvmmsg(connected, batch.entries, 1, MSG_OOB | MSG_DONTWAIT, NULL);
    mark("OOB_RECVMMSG_STREAM_EINVAL",
         stream_ready && count == -1 && errno == EINVAL);
    if (connected >= 0) {
        close(connected);
    }
    if (listener >= 0) {
        close(listener);
    }
    done();
}

/* MSG_WAITFORONE turns on MSG_DONTWAIT after the first datagram, so a blocking
 * socket returns as soon as one record is available. */
static void case_recvmmsg_waitforone(void) {
    begin("socket_msg.recvmmsg_waitforone.raw-differential");
    int receiver = -1;
    int sender = -1;
    udp_pair(&receiver, &sender);

    errno = 0;
    mark("WAITFORONE_QUEUED", send(sender, "q", 1, 0) == 1);
    struct batch batch;
    reset_batch(&batch);
    errno = 0;
    int count = recvmmsg(receiver, batch.entries, BATCH_LENGTH, MSG_WAITFORONE, NULL);
    mark("WAITFORONE_ONE_DATAGRAM", count == 1 && batch.entries[0].msg_len == 1);

    close(receiver);
    close(sender);
    done();
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
    signal(SIGPIPE, SIG_IGN);
    signal(SIGALRM, on_alarm);
    alarm(120);

    if (case_selected("socket_msg.send_flags")) {
        case_send_flags();
    }
    if (case_selected("socket_msg.sendmmsg_flags")) {
        case_sendmmsg_flags();
    }
    if (case_selected("socket_msg.peek_waitall_tcp")) {
        case_peek_waitall_tcp();
    }
    if (case_selected("socket_msg.tcp_more")) {
        case_tcp_more();
    }
    if (case_selected("socket_msg.compat_flag")) {
        case_compat_flag();
    }
    if (case_selected("socket_msg.waitall_stream")) {
        case_waitall_stream();
    }
    if (case_selected("socket_msg.waitall_tcp")) {
        case_waitall_tcp();
    }
    if (case_selected("socket_msg.waitall_datagram")) {
        case_waitall_datagram();
    }
    if (case_selected("socket_msg.recvmmsg_deadline")) {
        case_recvmmsg_deadline();
    }
    if (case_selected("socket_msg.recvmmsg_waitforone")) {
        case_recvmmsg_waitforone();
    }

    puts("THEKERNEL_SOCKET_MSG_OK");
    return 0;
}
