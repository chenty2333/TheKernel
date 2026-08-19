#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <linux/if_ether.h>
#include <linux/if_packet.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "packet_perf_peer requires the x86_64 Linux ABI"
#endif

#define PACKET_PROTOCOL UINT16_C(0x88b7)
#define WIRE_VERSION UINT16_C(1)
#define WIRE_HEADER_LEN 36U
#define ETH_FRAME_MIN 60U
#define MAX_PAYLOAD 2048U
#define MAX_FRAME 4096U
#define MAX_STREAM_PACKETS 100000U
#define MAX_ECHO_REQUESTS 100000U
#define HELLO_MAGIC UINT32_C(0x48454c4f)
#define HELLO_LEN 24U
#define PEER_BATCH 32U
#define PEER_SCHEMA "thekernel-tkpfnet1-peer-v1"

/* TKPFNET1 is the guest helper's little-endian Ethernet wire contract.  The
 * checksum covers wire bytes 0..31 and the declared payload, excluding the
 * checksum field itself.  HELLO carries command, filter mode, stream count,
 * payload length, stream base, and latency count as six u32 values. */

#define FLAG_STREAM UINT32_C(0x00000001)
#define FLAG_ECHO_REQUEST UINT32_C(0x00000002)
#define FLAG_ECHO_RESPONSE UINT32_C(0x00000004)
#define FLAG_HELLO UINT32_C(0x00000008)

enum filter_mode {
    FILTER_OFF = 0,
    FILTER_SHORT_ACCEPT = 1,
    FILTER_BRANCH_HALF = 2,
};

enum parse_result {
    PARSE_OK = 0,
    PARSE_MALFORMED = -1,
    PARSE_BAD_CHECKSUM = -2,
};

struct peer_config {
    char interface_name[IFNAMSIZ];
    bool interface_given;
    unsigned char peer_mac[ETH_ALEN];
    bool peer_mac_given;
    unsigned int backend_cpu;
    bool backend_cpu_given;
    uint64_t run_id;
    bool run_id_given;
    unsigned int hello_timeout_ms;
    unsigned int deadline_ms;
    unsigned int idle_timeout_ms;
};

struct wire_view {
    const unsigned char *frame;
    const unsigned char *wire;
    const unsigned char *payload;
    size_t frame_len;
    uint32_t flags;
    uint64_t run_id;
    uint32_t sequence;
    uint32_t payload_len;
    uint32_t checksum;
};

struct hello_values {
    enum filter_mode mode;
    uint32_t stream_count;
    uint32_t payload_len;
    uint32_t stream_base;
    uint32_t latency_count;
};

struct run_state {
    struct hello_values hello;
    unsigned char guest_mac[ETH_ALEN];
    uint32_t next_echo_sequence;
    uint64_t sent;
    uint64_t echoed;
    uint32_t checksum;
    unsigned int errors;
};

struct tx_batch {
    unsigned char frames[PEER_BATCH][MAX_FRAME];
    uint32_t checksums[PEER_BATCH];
    size_t lengths[PEER_BATCH];
    struct mmsghdr messages[PEER_BATCH];
    struct iovec vectors[PEER_BATCH];
};

static struct tx_batch tx_batch;
static unsigned char response_frame[MAX_FRAME];
static volatile sig_atomic_t stop_requested;

static void handle_signal(int signal_number) {
    (void)signal_number;
    stop_requested = 1;
}

static bool install_signal_handlers(void) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = handle_signal;
    if (sigemptyset(&action.sa_mask) != 0) {
        return false;
    }
    if (sigaction(SIGINT, &action, NULL) != 0 ||
        sigaction(SIGTERM, &action, NULL) != 0 ||
        sigaction(SIGHUP, &action, NULL) != 0) {
        return false;
    }
    return true;
}

static void usage(FILE *stream) {
    fprintf(stream,
            "Usage: packet_perf_peer [options]\n"
            "  --interface IFACE          tap interface (required)\n"
            "  --peer-mac MAC             peer MAC used by the guest (required)\n"
            "  --backend-cpu CPU          exact backend CPU (required)\n"
            "  --run-id HEX               16 hexadecimal run id (required)\n"
            "  --hello-timeout-ms N       HELLO deadline (default 5000)\n"
            "  --deadline-ms N            complete-run deadline (default 30000)\n"
            "  --idle-timeout-ms N       closed-loop idle deadline (default 1000)\n"
            "  --timeout-ms N             set all three deadlines\n"
            "  --selftest-codec           run codec checks without raw-socket access\n"
            "\n");
}

static bool parse_uint(const char *text, unsigned int *value,
                       unsigned int maximum, const char *option) {
    char *end = NULL;
    unsigned long parsed;
    errno = 0;
    parsed = strtoul(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || parsed == 0 ||
        parsed > maximum) {
        fprintf(stderr, "packet_perf_peer: invalid %s: %s\n", option, text);
        return false;
    }
    *value = (unsigned int)parsed;
    return true;
}

static bool parse_cpu(const char *text, unsigned int *value) {
    char *end = NULL;
    unsigned long parsed;
    errno = 0;
    parsed = strtoul(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' ||
        parsed >= (unsigned long)CPU_SETSIZE) {
        fprintf(stderr, "packet_perf_peer: invalid --backend-cpu: %s\n", text);
        return false;
    }
    *value = (unsigned int)parsed;
    return true;
}

static int hex_digit(char value) {
    if (value >= '0' && value <= '9') {
        return value - '0';
    }
    if (value >= 'a' && value <= 'f') {
        return value - 'a' + 10;
    }
    if (value >= 'A' && value <= 'F') {
        return value - 'A' + 10;
    }
    return -1;
}

static bool parse_hex64(const char *text, uint64_t *value) {
    uint64_t parsed = 0;
    if (strlen(text) != 16U) {
        return false;
    }
    for (size_t index = 0; index < 16U; ++index) {
        int digit = hex_digit(text[index]);
        if (digit < 0) {
            return false;
        }
        parsed = (parsed << 4) | (uint64_t)digit;
    }
    *value = parsed;
    return true;
}

static bool parse_mac(const char *text, unsigned char mac[ETH_ALEN]) {
    unsigned int bytes[ETH_ALEN];
    char tail;
    int count = sscanf(text, "%x:%x:%x:%x:%x:%x%c", &bytes[0], &bytes[1],
                       &bytes[2], &bytes[3], &bytes[4], &bytes[5], &tail);
    if (count != ETH_ALEN) {
        return false;
    }
    for (unsigned int index = 0; index < ETH_ALEN; ++index) {
        if (bytes[index] > 0xffU) {
            return false;
        }
        mac[index] = (unsigned char)bytes[index];
    }
    if ((mac[0] & 1U) != 0 || memcmp(mac, "\0\0\0\0\0\0", ETH_ALEN) == 0) {
        return false;
    }
    return true;
}

static bool option_value(int argc, char **argv, int *index,
                         const char *option, const char **value) {
    const char *argument = argv[*index];
    size_t length = strlen(option);
    if (strncmp(argument, option, length) == 0 && argument[length] == '=') {
        *value = argument + length + 1U;
        return true;
    }
    if (strcmp(argument, option) == 0) {
        if (*index + 1 >= argc) {
            fprintf(stderr, "packet_perf_peer: missing value for %s\n", option);
            return false;
        }
        *value = argv[++*index];
        return true;
    }
    return false;
}

static bool parse_args(int argc, char **argv, struct peer_config *config,
                       bool *selftest) {
    memset(config, 0, sizeof(*config));
    config->hello_timeout_ms = 5000U;
    config->deadline_ms = 30000U;
    config->idle_timeout_ms = 1000U;
    *selftest = false;
    for (int index = 1; index < argc; ++index) {
        const char *value = NULL;
        if (strcmp(argv[index], "--help") == 0 ||
            strcmp(argv[index], "-h") == 0) {
            usage(stdout);
            exit(EXIT_SUCCESS);
        }
        if (strcmp(argv[index], "--selftest-codec") == 0) {
            *selftest = true;
            continue;
        }
        if (option_value(argc, argv, &index, "--interface", &value)) {
            size_t length = strlen(value);
            if (length == 0 || length >= IFNAMSIZ ||
                strpbrk(value, " \t\r\n" ) != NULL) {
                fprintf(stderr, "packet_perf_peer: invalid --interface: %s\n",
                        value);
                return false;
            }
            memcpy(config->interface_name, value, length + 1U);
            config->interface_given = true;
            continue;
        }
        if (option_value(argc, argv, &index, "--peer-mac", &value) ||
            option_value(argc, argv, &index, "--guest-mac", &value)) {
            if (!parse_mac(value, config->peer_mac)) {
                fprintf(stderr, "packet_perf_peer: invalid peer MAC: %s\n",
                        value);
                return false;
            }
            config->peer_mac_given = true;
            continue;
        }
        if (option_value(argc, argv, &index, "--backend-cpu", &value)) {
            unsigned int cpu;
            if (!parse_cpu(value, &cpu)) {
                return false;
            }
            config->backend_cpu = cpu;
            config->backend_cpu_given = true;
            continue;
        }
        if (option_value(argc, argv, &index, "--run-id", &value)) {
            if (!parse_hex64(value, &config->run_id)) {
                fprintf(stderr, "packet_perf_peer: invalid --run-id: %s\n",
                        value);
                return false;
            }
            config->run_id_given = true;
            continue;
        }
        if (option_value(argc, argv, &index, "--hello-timeout-ms", &value)) {
            if (!parse_uint(value, &config->hello_timeout_ms, 600000U,
                            "--hello-timeout-ms")) {
                return false;
            }
            continue;
        }
        if (option_value(argc, argv, &index, "--deadline-ms", &value)) {
            if (!parse_uint(value, &config->deadline_ms, 600000U,
                            "--deadline-ms")) {
                return false;
            }
            continue;
        }
        if (option_value(argc, argv, &index, "--idle-timeout-ms", &value)) {
            if (!parse_uint(value, &config->idle_timeout_ms, 600000U,
                            "--idle-timeout-ms")) {
                return false;
            }
            continue;
        }
        if (option_value(argc, argv, &index, "--timeout-ms", &value)) {
            unsigned int timeout;
            if (!parse_uint(value, &timeout, 600000U, "--timeout-ms")) {
                return false;
            }
            config->hello_timeout_ms = timeout;
            config->deadline_ms = timeout;
            config->idle_timeout_ms = timeout;
            continue;
        }
        fprintf(stderr, "packet_perf_peer: unknown option: %s\n", argv[index]);
        return false;
    }
    if (*selftest) {
        if (argc != 2) {
            fprintf(stderr, "packet_perf_peer: --selftest-codec takes no options\n");
            return false;
        }
        return true;
    }
    if (!config->interface_given || !config->peer_mac_given ||
        !config->backend_cpu_given || !config->run_id_given) {
        fprintf(stderr,
                "packet_perf_peer: --interface, --peer-mac, --backend-cpu, "
                "and --run-id are required\n");
        return false;
    }
    return true;
}

static void put16(unsigned char *where, uint16_t value) {
    where[0] = (unsigned char)value;
    where[1] = (unsigned char)(value >> 8);
}

static void put32(unsigned char *where, uint32_t value) {
    for (unsigned int index = 0; index < 4U; ++index) {
        where[index] = (unsigned char)(value >> (index * 8U));
    }
}

static void put64(unsigned char *where, uint64_t value) {
    for (unsigned int index = 0; index < 8U; ++index) {
        where[index] = (unsigned char)(value >> (index * 8U));
    }
}

static uint16_t get16(const unsigned char *where) {
    return (uint16_t)where[0] | ((uint16_t)where[1] << 8);
}

static uint32_t get32(const unsigned char *where) {
    uint32_t value = 0;
    for (unsigned int index = 0; index < 4U; ++index) {
        value |= (uint32_t)where[index] << (index * 8U);
    }
    return value;
}

static uint64_t get64(const unsigned char *where) {
    uint64_t value = 0;
    for (unsigned int index = 0; index < 8U; ++index) {
        value |= (uint64_t)where[index] << (index * 8U);
    }
    return value;
}

static uint32_t fnv1a(const unsigned char *data, size_t length,
                      uint32_t hash) {
    for (size_t index = 0; index < length; ++index) {
        hash ^= data[index];
        hash *= UINT32_C(16777619);
    }
    return hash;
}

static uint32_t wire_checksum(const unsigned char *wire, size_t payload_len) {
    uint32_t hash = UINT32_C(2166136261);
    hash = fnv1a(wire, 32U, hash);
    return fnv1a(wire + WIRE_HEADER_LEN, payload_len, hash);
}

static void checksum_mix(uint32_t *state, uint32_t value) {
    unsigned char bytes[sizeof(value)];
    put32(bytes, value);
    *state = fnv1a(bytes, sizeof(bytes), *state);
}

static bool mac_equal(const unsigned char left[ETH_ALEN],
                      const unsigned char right[ETH_ALEN]) {
    return memcmp(left, right, ETH_ALEN) == 0;
}

static bool valid_source_mac(const unsigned char mac[ETH_ALEN]) {
    static const unsigned char zero_mac[ETH_ALEN] = {0};
    return (mac[0] & 1U) == 0 && !mac_equal(mac, zero_mac);
}

static size_t build_frame(unsigned char *frame, size_t capacity,
                          const unsigned char destination[ETH_ALEN],
                          const unsigned char source[ETH_ALEN],
                          uint64_t run_id, uint32_t sequence, uint32_t flags,
                          const unsigned char *payload,
                          unsigned int payload_len, uint32_t *checksum) {
    size_t wire_len = WIRE_HEADER_LEN + payload_len;
    size_t frame_len = ETH_HLEN + wire_len;
    unsigned char *wire;
    if (payload_len > MAX_PAYLOAD || frame_len > capacity ||
        frame_len > MAX_FRAME) {
        return 0;
    }
    if (frame_len < ETH_FRAME_MIN) {
        frame_len = ETH_FRAME_MIN;
    }
    memset(frame, 0, frame_len);
    memcpy(frame, destination, ETH_ALEN);
    memcpy(frame + ETH_ALEN, source, ETH_ALEN);
    frame[12] = (unsigned char)(PACKET_PROTOCOL >> 8);
    frame[13] = (unsigned char)PACKET_PROTOCOL;
    wire = frame + ETH_HLEN;
    memcpy(wire, "TKPFNET1", 8U);
    put16(wire + 8U, WIRE_VERSION);
    put16(wire + 10U, WIRE_HEADER_LEN);
    put32(wire + 12U, flags);
    put64(wire + 16U, run_id);
    put32(wire + 24U, sequence);
    put32(wire + 28U, payload_len);
    if (payload != NULL) {
        memcpy(wire + WIRE_HEADER_LEN, payload, payload_len);
    } else {
        for (unsigned int index = 0; index < payload_len; ++index) {
            wire[WIRE_HEADER_LEN + index] =
                (unsigned char)(sequence + index * 17U);
        }
    }
    uint32_t value = wire_checksum(wire, payload_len);
    put32(wire + 32U, value);
    if (checksum != NULL) {
        *checksum = value;
    }
    return frame_len;
}

static int parse_frame(const unsigned char *frame, size_t frame_len,
                       struct wire_view *view) {
    const unsigned char *wire;
    size_t required;
    uint32_t expected;
    uint32_t actual;
    if (frame_len < ETH_HLEN + WIRE_HEADER_LEN ||
        ((uint16_t)frame[12] << 8 | frame[13]) != PACKET_PROTOCOL) {
        return PARSE_MALFORMED;
    }
    wire = frame + ETH_HLEN;
    if (memcmp(wire, "TKPFNET1", 8U) != 0 ||
        get16(wire + 8U) != WIRE_VERSION ||
        get16(wire + 10U) != WIRE_HEADER_LEN) {
        return PARSE_MALFORMED;
    }
    uint32_t payload_len = get32(wire + 28U);
    if (payload_len > MAX_PAYLOAD) {
        return PARSE_MALFORMED;
    }
    required = ETH_HLEN + WIRE_HEADER_LEN + (size_t)payload_len;
    size_t expected_frame_len = required < ETH_FRAME_MIN ? ETH_FRAME_MIN : required;
    if (frame_len != expected_frame_len) {
        return PARSE_MALFORMED;
    }
    expected = get32(wire + 32U);
    actual = wire_checksum(wire, payload_len);
    view->frame = frame;
    view->wire = wire;
    view->payload = wire + WIRE_HEADER_LEN;
    view->frame_len = frame_len;
    view->flags = get32(wire + 12U);
    view->run_id = get64(wire + 16U);
    view->sequence = get32(wire + 24U);
    view->payload_len = payload_len;
    view->checksum = actual;
    return expected == actual ? PARSE_OK : PARSE_BAD_CHECKSUM;
}

static bool payload_matches_pattern(const struct wire_view *view) {
    for (uint32_t index = 0; index < view->payload_len; ++index) {
        if (view->payload[index] !=
            (unsigned char)(view->sequence + index * 17U)) {
            return false;
        }
    }
    return true;
}

static uint64_t now_ns(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) {
        return 0;
    }
    return (uint64_t)value.tv_sec * UINT64_C(1000000000) +
           (uint64_t)value.tv_nsec;
}

static uint64_t deadline_after(uint64_t start, unsigned int milliseconds) {
    uint64_t offset = (uint64_t)milliseconds * UINT64_C(1000000);
    if (UINT64_MAX - start < offset) {
        return UINT64_MAX;
    }
    return start + offset;
}

static uint64_t earlier_deadline(uint64_t left, uint64_t right) {
    return left < right ? left : right;
}

static int remaining_timeout_ms(uint64_t deadline) {
    uint64_t current = now_ns();
    uint64_t remaining;
    uint64_t milliseconds;
    if (current == 0 || current >= deadline) {
        return 0;
    }
    remaining = deadline - current;
    milliseconds = (remaining + UINT64_C(999999)) / UINT64_C(1000000);
    if (milliseconds > (uint64_t)INT_MAX) {
        return INT_MAX;
    }
    return milliseconds == 0 ? 1 : (int)milliseconds;
}

static bool wait_fd(int fd, short events, uint64_t deadline) {
    struct pollfd descriptor = {.fd = fd, .events = events, .revents = 0};
    for (;;) {
        int timeout = remaining_timeout_ms(deadline);
        if (stop_requested || timeout == 0) {
            errno = stop_requested ? EINTR : ETIMEDOUT;
            return false;
        }
        int result = poll(&descriptor, 1, timeout);
        if (result > 0) {
            if ((descriptor.revents & events) != 0) {
                return true;
            }
            if ((descriptor.revents & (POLLERR | POLLHUP | POLLNVAL)) != 0) {
                errno = EIO;
                return false;
            }
            descriptor.revents = 0;
            continue;
        }
        if (result == 0) {
            errno = ETIMEDOUT;
            return false;
        }
        if (errno == EINTR) {
            continue;
        }
        return false;
    }
}

static bool send_batch(int fd, unsigned int ifindex,
                       const unsigned char destination[ETH_ALEN],
                       unsigned int count, uint64_t deadline,
                       struct run_state *state) {
    struct sockaddr_ll address;
    unsigned int offset = 0;
    memset(&address, 0, sizeof(address));
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(PACKET_PROTOCOL);
    address.sll_ifindex = (int)ifindex;
    address.sll_halen = ETH_ALEN;
    memcpy(address.sll_addr, destination, ETH_ALEN);
    for (unsigned int index = 0; index < count; ++index) {
        memset(&tx_batch.messages[index], 0, sizeof(tx_batch.messages[index]));
        tx_batch.vectors[index].iov_base = tx_batch.frames[index];
        tx_batch.vectors[index].iov_len = tx_batch.lengths[index];
        tx_batch.messages[index].msg_hdr.msg_name = &address;
        tx_batch.messages[index].msg_hdr.msg_namelen = sizeof(address);
        tx_batch.messages[index].msg_hdr.msg_iov = &tx_batch.vectors[index];
        tx_batch.messages[index].msg_hdr.msg_iovlen = 1;
    }
    while (offset < count) {
        int result;
        if (stop_requested) {
            errno = EINTR;
            return false;
        }
        result = sendmmsg(fd, &tx_batch.messages[offset], count - offset, 0);
        if (result > 0) {
            for (int index = 0; index < result; ++index) {
                checksum_mix(&state->checksum,
                             tx_batch.checksums[offset + (unsigned int)index]);
            }
            state->sent += (uint64_t)result;
            offset += (unsigned int)result;
            continue;
        }
        if (result < 0 && (errno == EINTR || errno == EAGAIN ||
                           errno == EWOULDBLOCK)) {
            if (!wait_fd(fd, POLLOUT, deadline)) {
                return false;
            }
            continue;
        }
        return false;
    }
    return true;
}

static bool send_response(int fd, unsigned int ifindex,
                          const unsigned char destination[ETH_ALEN],
                          const unsigned char source[ETH_ALEN], uint64_t run_id,
                          const struct wire_view *request, uint64_t deadline,
                          struct run_state *state) {
    struct sockaddr_ll address;
    uint32_t checksum = 0;
    size_t frame_len = build_frame(response_frame, sizeof(response_frame),
                                   destination, source, run_id,
                                   request->sequence, FLAG_ECHO_RESPONSE,
                                   request->payload,
                                   (unsigned int)request->payload_len,
                                   &checksum);
    if (frame_len == 0) {
        errno = EINVAL;
        return false;
    }
    memset(&address, 0, sizeof(address));
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(PACKET_PROTOCOL);
    address.sll_ifindex = (int)ifindex;
    address.sll_halen = ETH_ALEN;
    memcpy(address.sll_addr, destination, ETH_ALEN);
    for (;;) {
        ssize_t result;
        if (stop_requested) {
            errno = EINTR;
            return false;
        }
        result = sendto(fd, response_frame, frame_len, 0,
                        (const struct sockaddr *)&address, sizeof(address));
        if (result == (ssize_t)frame_len) {
            checksum_mix(&state->checksum, checksum);
            state->sent++;
            state->echoed++;
            return true;
        }
        if (result < 0 && (errno == EINTR || errno == EAGAIN ||
                           errno == EWOULDBLOCK)) {
            if (!wait_fd(fd, POLLOUT, deadline)) {
                return false;
            }
            continue;
        }
        if (result >= 0) {
            errno = EIO;
        }
        return false;
    }
}

static bool send_stream(int fd, unsigned int ifindex,
                        const unsigned char destination[ETH_ALEN],
                        const unsigned char source[ETH_ALEN], uint64_t run_id,
                        const struct hello_values *hello, uint64_t deadline,
                        struct run_state *state) {
    uint32_t sequence = hello->stream_base;
    uint32_t remaining = hello->stream_count;
    while (remaining != 0) {
        unsigned int count = remaining > PEER_BATCH ? PEER_BATCH : remaining;
        for (unsigned int index = 0; index < count; ++index) {
            uint32_t checksum = 0;
            size_t length = build_frame(
                tx_batch.frames[index], sizeof(tx_batch.frames[index]),
                destination, source, run_id, sequence + index, FLAG_STREAM,
                NULL, hello->payload_len, &checksum);
            if (length == 0) {
                errno = EINVAL;
                return false;
            }
            tx_batch.lengths[index] = length;
            tx_batch.checksums[index] = checksum;
        }
        if (!send_batch(fd, ifindex, destination, count, deadline, state)) {
            return false;
        }
        sequence += count;
        remaining -= count;
    }
    return true;
}

static bool receive_one(int fd, unsigned char frame[MAX_FRAME], size_t *length,
                        uint64_t deadline) {
    for (;;) {
        if (!wait_fd(fd, POLLIN, deadline)) {
            return false;
        }
        ssize_t result = recv(fd, frame, MAX_FRAME, 0);
        if (result >= 0) {
            *length = (size_t)result;
            return true;
        }
        if (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK) {
            continue;
        }
        return false;
    }
}

static bool validate_hello(const struct wire_view *view,
                           const struct peer_config *config,
                           struct hello_values *hello) {
    uint32_t mode;
    if (view->run_id != config->run_id || view->flags != FLAG_HELLO ||
        view->payload_len != HELLO_LEN) {
        return false;
    }
    if (get32(view->payload) != HELLO_MAGIC) {
        return false;
    }
    mode = get32(view->payload + 4U);
    if (mode > FILTER_BRANCH_HALF) {
        return false;
    }
    hello->mode = (enum filter_mode)mode;
    hello->stream_count = get32(view->payload + 8U);
    hello->payload_len = get32(view->payload + 12U);
    hello->stream_base = get32(view->payload + 16U);
    hello->latency_count = get32(view->payload + 20U);
    if (hello->stream_count == 0 || hello->stream_count > MAX_STREAM_PACKETS ||
        hello->payload_len == 0 || hello->payload_len > MAX_PAYLOAD ||
        hello->latency_count == 0 || hello->latency_count > MAX_ECHO_REQUESTS) {
        return false;
    }
    if ((uint64_t)hello->stream_base + hello->stream_count > UINT32_MAX) {
        return false;
    }
    if ((hello->mode == FILTER_BRANCH_HALF &&
         (hello->stream_base & 1U) == 0) ||
        (hello->mode != FILTER_BRANCH_HALF &&
         (hello->stream_base & 1U) != 0)) {
        return false;
    }
    if (view->sequence != (hello->stream_base | 1U)) {
        return false;
    }
    uint64_t first_echo = (uint64_t)hello->stream_base +
                          hello->stream_count + UINT64_C(1001);
    if (first_echo > UINT32_MAX) {
        return false;
    }
    if (hello->mode == FILTER_BRANCH_HALF) {
        first_echo |= 1U;
        if (first_echo > UINT32_MAX) {
            return false;
        }
    }
    return true;
}

static bool frame_for_guest(const struct wire_view *view,
                            const struct run_state *state,
                            const struct peer_config *config) {
    return mac_equal(view->frame, config->peer_mac) &&
           mac_equal(view->frame + ETH_ALEN, state->guest_mac) &&
           view->run_id == config->run_id;
}

static bool receive_hello(int fd, const struct peer_config *config,
                          struct run_state *state, uint64_t deadline) {
    static unsigned char frame[MAX_FRAME];
    for (;;) {
        size_t length = 0;
        struct wire_view view;
        int parsed;
        if (stop_requested || !receive_one(fd, frame, &length, deadline)) {
            return false;
        }
        if (length < ETH_HLEN) {
            errno = EPROTO;
            return false;
        }
        if (!mac_equal(frame, config->peer_mac) ||
            mac_equal(frame + ETH_ALEN, config->peer_mac)) {
            continue;
        }
        if (!valid_source_mac(frame + ETH_ALEN)) {
            errno = EPROTO;
            return false;
        }
        parsed = parse_frame(frame, length, &view);
        if (parsed != PARSE_OK ||
            !validate_hello(&view, config, &state->hello)) {
            errno = EPROTO;
            return false;
        }
        memcpy(state->guest_mac, frame + ETH_ALEN, ETH_ALEN);
        state->next_echo_sequence = state->hello.stream_base +
                                    state->hello.stream_count + 1001U;
        if (state->hello.mode == FILTER_BRANCH_HALF) {
            state->next_echo_sequence |= 1U;
        }
        state->checksum = UINT32_C(2166136261);
        checksum_mix(&state->checksum, view.checksum);
        return true;
    }
}

static bool receive_echoes(int fd, unsigned int ifindex,
                           const struct peer_config *config,
                           struct run_state *state, uint64_t deadline) {
    static unsigned char frame[MAX_FRAME];
    uint64_t idle_deadline = deadline;
    bool threshold_reached = false;
    for (;;) {
        size_t length = 0;
        struct wire_view view;
        int parsed;
        uint64_t receive_deadline = threshold_reached ? idle_deadline : deadline;
        if (stop_requested) {
            errno = EINTR;
            return false;
        }
        if (!receive_one(fd, frame, &length, receive_deadline)) {
            return threshold_reached && errno == ETIMEDOUT;
        }
        if (length < ETH_HLEN) {
            errno = EPROTO;
            return false;
        }
        if (!mac_equal(frame, config->peer_mac) ||
            !mac_equal(frame + ETH_ALEN, state->guest_mac)) {
            continue;
        }
        parsed = parse_frame(frame, length, &view);
        if (parsed != PARSE_OK || !frame_for_guest(&view, state, config) ||
            view.flags != FLAG_ECHO_REQUEST ||
            view.payload_len != state->hello.payload_len ||
            view.sequence != state->next_echo_sequence ||
            !payload_matches_pattern(&view)) {
            errno = EPROTO;
            return false;
        }
        checksum_mix(&state->checksum, view.checksum);
        if (!send_response(fd, ifindex, state->guest_mac, config->peer_mac,
                           config->run_id, &view, deadline, state)) {
            return false;
        }
        uint64_t next_sequence = (uint64_t)state->next_echo_sequence +
                                 (state->hello.mode == FILTER_BRANCH_HALF ? 2U : 1U);
        if (next_sequence > UINT32_MAX || state->echoed >= MAX_ECHO_REQUESTS) {
            errno = EOVERFLOW;
            return false;
        }
        state->next_echo_sequence = (uint32_t)next_sequence;
        if (state->echoed >= state->hello.latency_count) {
            threshold_reached = true;
            idle_deadline = earlier_deadline(
                deadline, deadline_after(now_ns(), config->idle_timeout_ms));
        }
    }
}

static int open_packet_socket(unsigned int ifindex) {
    int fd = socket(AF_PACKET, SOCK_RAW | SOCK_NONBLOCK | SOCK_CLOEXEC,
                    htons(PACKET_PROTOCOL));
    struct sockaddr_ll address;
    if (fd < 0) {
        return -1;
    }
    memset(&address, 0, sizeof(address));
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(PACKET_PROTOCOL);
    address.sll_ifindex = (int)ifindex;
    if (bind(fd, (const struct sockaddr *)&address, sizeof(address)) != 0) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return -1;
    }
    return fd;
}

static bool set_exact_affinity(unsigned int cpu) {
    cpu_set_t requested;
    cpu_set_t actual;
    CPU_ZERO(&requested);
    CPU_SET(cpu, &requested);
    if (sched_setaffinity(0, sizeof(requested), &requested) != 0) {
        return false;
    }
    CPU_ZERO(&actual);
    if (sched_getaffinity(0, sizeof(actual), &actual) != 0 ||
        CPU_COUNT(&actual) != 1 || !CPU_ISSET(cpu, &actual)) {
        errno = EINVAL;
        return false;
    }
    return true;
}

static bool query_ifindex(const char *name, unsigned int *ifindex) {
    unsigned int value = if_nametoindex(name);
    if (value == 0) {
        errno = ENODEV;
        return false;
    }
    *ifindex = value;
    return true;
}

static int run_codec_selftest(void) {
    static const unsigned char destination[ETH_ALEN] =
        {0x02, 0x00, 0x00, 0x00, 0x00, 0x01};
    static const unsigned char source[ETH_ALEN] =
        {0x02, 0x00, 0x00, 0x00, 0x00, 0x02};
    unsigned char frame[MAX_FRAME];
    unsigned char payload[HELLO_LEN];
    struct wire_view view;
    size_t length;
    uint32_t checksum;
    memset(payload, 0, sizeof(payload));
    put32(payload, HELLO_MAGIC);
    put32(payload + 4U, FILTER_BRANCH_HALF);
    put32(payload + 8U, 7U);
    put32(payload + 12U, 64U);
    put32(payload + 16U, 1001U);
    put32(payload + 20U, 64U);
    length = build_frame(frame, sizeof(frame), destination, source,
                         UINT64_C(0x0123456789abcdef), 1001U | 1U,
                         FLAG_HELLO, payload, HELLO_LEN, &checksum);
    if (length == 0 || parse_frame(frame, length, &view) != PARSE_OK ||
        view.run_id != UINT64_C(0x0123456789abcdef) ||
        view.sequence != 1001U || view.checksum != checksum ||
        view.payload_len != HELLO_LEN || get32(view.payload + 4U) != 2U) {
        fprintf(stderr, "packet_perf_peer: codec selftest HELLO failed\n");
        return EXIT_FAILURE;
    }
    frame[ETH_HLEN + WIRE_HEADER_LEN + 3U] ^= 1U;
    if (parse_frame(frame, length, &view) != PARSE_BAD_CHECKSUM) {
        fprintf(stderr, "packet_perf_peer: codec selftest checksum failed\n");
        return EXIT_FAILURE;
    }
    length = build_frame(frame, sizeof(frame), destination, source,
                         UINT64_C(0x0123456789abcdef), 1234U,
                         FLAG_ECHO_REQUEST, NULL, 64U, &checksum);
    if (length == 0 || parse_frame(frame, length, &view) != PARSE_OK ||
        !payload_matches_pattern(&view) || view.flags != FLAG_ECHO_REQUEST ||
        view.sequence != 1234U || view.payload_len != 64U) {
        fprintf(stderr, "packet_perf_peer: codec selftest ECHO failed\n");
        return EXIT_FAILURE;
    }
    printf("packet_perf_peer selftest-codec status=ok\n");
    return EXIT_SUCCESS;
}

static void emit_ready(const struct peer_config *config) {
    printf("TKPFNET1_PEER_READY schema=%s run_id=%016" PRIx64
           " interface=%s mac=%02x:%02x:%02x:%02x:%02x:%02x status=ok\n",
           PEER_SCHEMA, config->run_id, config->interface_name,
           config->peer_mac[0], config->peer_mac[1], config->peer_mac[2],
           config->peer_mac[3], config->peer_mac[4], config->peer_mac[5]);
    fflush(stdout);
}

static void emit_done(const struct peer_config *config,
                      const struct run_state *state, bool success) {
    printf("TKPFNET1_PEER_DONE run_id=%016" PRIx64 " status=%s sent=%" PRIu64
           " echoed=%" PRIu64 " checksum=%08" PRIx32 " errors=%u\n",
           config->run_id, success ? "ok" : "fail", state->sent,
           state->echoed, state->checksum, state->errors);
    fflush(stdout);
}

int main(int argc, char **argv) {
    struct peer_config config;
    struct run_state state;
    bool selftest;
    unsigned int ifindex;
    int fd = -1;
    bool success = false;
    const char *failure = NULL;

    if (!parse_args(argc, argv, &config, &selftest)) {
        usage(stderr);
        return EXIT_FAILURE;
    }
    if (selftest) {
        return run_codec_selftest();
    }
    if (!install_signal_handlers()) {
        fprintf(stderr, "packet_perf_peer: cannot install signal handlers: %s\n",
                strerror(errno));
        return EXIT_FAILURE;
    }
    if (!query_ifindex(config.interface_name, &ifindex)) {
        fprintf(stderr, "packet_perf_peer: interface lookup failed: %s\n",
                strerror(errno));
        return EXIT_FAILURE;
    }
    fd = open_packet_socket(ifindex);
    if (fd < 0) {
        fprintf(stderr, "packet_perf_peer: AF_PACKET bind failed: %s\n",
                strerror(errno));
        return EXIT_FAILURE;
    }
    if (!set_exact_affinity(config.backend_cpu)) {
        fprintf(stderr, "packet_perf_peer: backend CPU affinity failed: %s\n",
                strerror(errno));
        close(fd);
        return EXIT_FAILURE;
    }
    emit_ready(&config);
    memset(&state, 0, sizeof(state));
    state.checksum = UINT32_C(2166136261);
    uint64_t operation_deadline = deadline_after(now_ns(), config.deadline_ms);
    uint64_t hello_deadline = earlier_deadline(
        operation_deadline, deadline_after(now_ns(), config.hello_timeout_ms));
    if (!receive_hello(fd, &config, &state, hello_deadline)) {
        failure = stop_requested ? "signal" :
                  (errno == ETIMEDOUT ? "HELLO timeout" : "invalid HELLO");
    } else if (!send_stream(fd, ifindex, state.guest_mac, config.peer_mac,
                           config.run_id, &state.hello, operation_deadline,
                           &state)) {
        failure = stop_requested ? "signal" : "STREAM send failed";
    } else if (!receive_echoes(fd, ifindex, &config, &state,
                              operation_deadline)) {
        failure = stop_requested ? "signal" :
                  (errno == ETIMEDOUT ? "ECHO timeout" : "invalid ECHO");
    } else {
        success = true;
    }
    if (!success) {
        state.errors = state.errors == UINT_MAX ? UINT_MAX : state.errors + 1U;
        fprintf(stderr, "packet_perf_peer: %s\n",
                failure == NULL ? "peer failure" : failure);
    }
    emit_done(&config, &state, success);
    close(fd);
    return success ? EXIT_SUCCESS : EXIT_FAILURE;
}
