#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/filter.h>
#include <linux/if_ether.h>
#include <linux/if_packet.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <poll.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "packet performance helper requires the x86_64 Linux ABI"
#endif

#define PERF_SCHEMA "thekernel-perf-v1"
#define PERF_RUN "TKPERF_RUN"
#define PERF_CORRECTNESS "TKPERF_CORRECTNESS"
#define PERF_WINDOW "TKPERF_WINDOW"
#define PERF_LATENCY "TKPERF_LATENCY"
#define PERF_DONE "TKPERF_DONE"

#define PACKET_PROTOCOL 0x88b7U
#define WIRE_VERSION 1U
#define WIRE_HEADER_LEN 36U
#define MAX_PAYLOAD 2048U
#define MAX_FRAME 4096U
#define MAX_STREAM_PACKETS 100000U
#define MAX_LATENCY_SAMPLES 1024U
#define DEFAULT_PAYLOAD 64U
#define DEFAULT_STREAM_PACKETS 64U
#define DEFAULT_WARMUP 8U
#define DEFAULT_LATENCY_SAMPLES 64U
#define DEFAULT_TIMEOUT_MS 1000U

#define FLAG_STREAM 0x00000001U
#define FLAG_ECHO_REQUEST 0x00000002U
#define FLAG_ECHO_RESPONSE 0x00000004U
#define FLAG_HELLO 0x00000008U

#define HELLO_MAGIC 0x48454c4fU
#define HELLO_LEN 24U
#define BPF_CONTROL_PATH "/proc/bpf_executor_control"
#define BPF_STATS_PATH "/proc/bpf_stats"
#define BPF_STATS_BUFFER 4096U

/*
 * Host-peer wire contract (all integer fields are little-endian):
 * Ethernet, EtherType 0x88b7, then magic[8]="TKPFNET1", version:u16,
 * header_len:u16 (36), flags:u32, run_id:u64, seq:u32, payload_len:u32,
 * checksum:u32.  The checksum is FNV-1a over header bytes 0..31 followed
 * by the declared payload.  A HELLO payload is command 0x48454c4f, filter
 * mode, stream count, payload length, stream base, and latency count (six
 * u32 values).  The peer sends STREAM frames from stream_base and answers
 * ECHO_REQUEST with an ECHO_RESPONSE carrying the same run_id/seq/payload.
 */

enum filter_mode {
    FILTER_OFF = 0,
    FILTER_SHORT_ACCEPT = 1,
    FILTER_BRANCH_HALF = 2,
};

enum executor_mode {
    EXECUTOR_AUTO,
    EXECUTOR_INTERPRETER,
    EXECUTOR_JIT,
};

struct bpf_counters {
    uint64_t published;
    uint64_t native_executed;
    uint64_t interpreter_executed;
    uint64_t fallback_policy_interpreter;
    uint64_t fallback_translation;
    uint64_t fallback_publication;
    uint64_t fallback_owner;
    uint64_t fallback_unavailable;
    uint64_t jit_rejected;
};

struct bpf_delta {
    struct bpf_counters values;
    bool available;
    bool valid;
};

struct executor_config {
    enum executor_mode mode;
    int control_state;
    int stats_state;
};

struct cell_proof {
    struct bpf_delta delta;
    const char *kind;
    const char *reason;
    bool accepted;
};

struct interface_info {
    char name[IFNAMSIZ];
    unsigned int ifindex;
    unsigned char mac[ETH_ALEN];
    bool loopback;
};

struct config {
    const char *interface_name;
    bool interface_given;
    bool formal;
    bool mode_given;
    enum filter_mode selected_filter;
    bool filter_given;
    unsigned int payload_len;
    unsigned int stream_packets;
    unsigned int warmup;
    unsigned int latency_samples;
    unsigned int timeout_ms;
    unsigned char peer_mac[ETH_ALEN];
    bool peer_given;
    uint64_t run_id;
    bool run_id_given;
    enum executor_mode executor;
    bool executor_given;
};

struct stream_stats {
    unsigned int offered;
    unsigned int expected;
    unsigned int accepted;
    unsigned int rejected;
    unsigned int missing;
    unsigned int duplicate;
    unsigned int checksum;
    uint64_t bytes;
    uint64_t elapsed_ns;
};

struct latency_stats {
    uint64_t wall[MAX_LATENCY_SAMPLES];
    uint64_t cpu[MAX_LATENCY_SAMPLES];
    unsigned int count;
};

struct parsed_packet {
    uint64_t run_id;
    uint32_t seq;
    uint32_t payload_len;
    uint32_t flags;
};

static void usage(FILE *stream) {
    fprintf(stream,
            "Usage: packet-perf [--selftest|--formal] [--interface IFACE] "
            "[options]\n"
            "  --interface IFACE       AF_PACKET benchmark NIC\n"
            "  --formal                host-peer mode (loopback is rejected)\n"
            "  --selftest              explicit local AF_PACKET correctness mode\n"
            "  --filter off|short-accept|branch-select-half\n"
            "  --peer-mac aa:bb:cc:dd:ee:ff  formal peer destination\n"
            "  --frame-length N        payload length (default 64)\n"
            "  --stream-packets N      offered stream packets (default 64)\n"
            "  --warmup N              latency warmup requests (default 8)\n"
            "  --latency-samples N     latency samples (default 64)\n"
            "  --timeout-ms N          receive timeout (default 1000)\n"
            "  --run-id HEX            16-hex-digit run id\n"
            "  --executor auto|interpreter|jit  cBPF executor policy\n");
}

static void error_line(const char *stage, const char *reason) {
    fprintf(stderr, "PACKET_PERF_ERROR stage=%s reason=%s errno=%d (%s)\n",
            stage, reason, errno, strerror(errno));
}

static const char *executor_name(enum executor_mode mode) {
    switch (mode) {
    case EXECUTOR_AUTO:
        return "auto";
    case EXECUTOR_INTERPRETER:
        return "interpreter";
    case EXECUTOR_JIT:
        return "jit";
    }
    return "unknown";
}

static bool parse_executor(const char *text, enum executor_mode *mode) {
    if (strcmp(text, "auto") == 0) {
        *mode = EXECUTOR_AUTO;
        return true;
    }
    if (strcmp(text, "interpreter") == 0) {
        *mode = EXECUTOR_INTERPRETER;
        return true;
    }
    if (strcmp(text, "jit") == 0) {
        *mode = EXECUTOR_JIT;
        return true;
    }
    return false;
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

static int control_readback(const char *domain, enum executor_mode mode) {
    char buffer[BPF_STATS_BUFFER];
    int result = read_proc_text(BPF_CONTROL_PATH, buffer, sizeof(buffer));
    if (result != 1) {
        return result;
    }
    char *save = NULL;
    bool found = false;
    for (char *line = strtok_r(buffer, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        char key[32];
        char value[32];
        char extra[2];
        if (sscanf(line, "%31[^=]=%31s %1s", key, value, extra) != 2) {
            continue;
        }
        if (strcmp(key, domain) == 0) {
            found = strcmp(value, executor_name(mode)) == 0;
        }
    }
    return found ? 1 : -1;
}

static int set_executor_control(const char *domain, enum executor_mode mode) {
    int fd = open(BPF_CONTROL_PATH, O_WRONLY);
    if (fd < 0) {
        if (errno == ENOENT || errno == ENOTDIR) {
            return 0;
        }
        return -1;
    }
    char request[64];
    int length = snprintf(request, sizeof(request), "%s=%s\n", domain,
                          executor_name(mode));
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
    return control_readback(domain, mode);
}

static int parse_counter(const char *text, uint64_t *value) {
    char *end = NULL;
    errno = 0;
    unsigned long long parsed = strtoull(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0') {
        return -1;
    }
    *value = (uint64_t)parsed;
    return 0;
}

static int read_bpf_stats(const char *domain, struct bpf_counters *counters) {
    char buffer[BPF_STATS_BUFFER];
    int result = read_proc_text(BPF_STATS_PATH, buffer, sizeof(buffer));
    if (result != 1) {
        return result;
    }
    memset(counters, 0, sizeof(*counters));
    bool seen[9] = {false};
    char *save = NULL;
    size_t domain_length = strlen(domain);
    for (char *line = strtok_r(buffer, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        if (strncmp(line, "BPF_STATS ", 11) == 0) {
            continue;
        }
        char key[64];
        char value[64];
        char extra[2];
        if (sscanf(line, "%63s %63s %1s", key, value, extra) != 2) {
            errno = EPROTO;
            return -1;
        }
        if (strncmp(key, domain, domain_length) != 0 ||
            key[domain_length] != '.') {
            continue;
        }
        const char *field = key + domain_length + 1U;
        uint64_t *destination = NULL;
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
        if (destination == NULL || seen[index] || parse_counter(value, destination) != 0) {
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

static int prepare_executor(const char *domain, enum executor_mode mode,
                            struct executor_config *config) {
    config->mode = mode;
    config->control_state = set_executor_control(domain, mode);
    config->stats_state = read_bpf_stats(domain, &(struct bpf_counters){0});
    if (config->control_state < 0 || config->stats_state < 0) {
        return -1;
    }
    if (mode != EXECUTOR_AUTO &&
        (config->control_state != 1 || config->stats_state != 1)) {
        errno = ENOTSUP;
        return 0;
    }
    return 1;
}

static bool subtract_counter(uint64_t after, uint64_t before, uint64_t *delta) {
    if (after < before) {
        return false;
    }
    *delta = after - before;
    return true;
}

static bool make_delta(const struct bpf_counters *before,
                       const struct bpf_counters *after,
                       struct bpf_delta *delta) {
    memset(delta, 0, sizeof(*delta));
    delta->available = true;
    delta->valid =
        subtract_counter(after->published, before->published,
                         &delta->values.published) &&
        subtract_counter(after->native_executed, before->native_executed,
                         &delta->values.native_executed) &&
        subtract_counter(after->interpreter_executed, before->interpreter_executed,
                         &delta->values.interpreter_executed) &&
        subtract_counter(after->fallback_policy_interpreter,
                         before->fallback_policy_interpreter,
                         &delta->values.fallback_policy_interpreter) &&
        subtract_counter(after->fallback_translation, before->fallback_translation,
                         &delta->values.fallback_translation) &&
        subtract_counter(after->fallback_publication, before->fallback_publication,
                         &delta->values.fallback_publication) &&
        subtract_counter(after->fallback_owner, before->fallback_owner,
                         &delta->values.fallback_owner) &&
        subtract_counter(after->fallback_unavailable, before->fallback_unavailable,
                         &delta->values.fallback_unavailable) &&
        subtract_counter(after->jit_rejected, before->jit_rejected,
                         &delta->values.jit_rejected);
    return delta->valid;
}

static uint64_t fallback_total(const struct bpf_counters *values) {
    return values->fallback_policy_interpreter + values->fallback_translation +
           values->fallback_publication + values->fallback_owner +
           values->fallback_unavailable;
}

static struct cell_proof evaluate_proof(enum filter_mode filter,
                                        const struct executor_config *config,
                                        const struct bpf_delta *delta,
                                        bool correctness_ok, bool filter_installed) {
    struct cell_proof proof = {.delta = *delta, .kind = "unsupported-ablation",
                               .reason = "bpf-stats-unavailable", .accepted = false};
    if (filter == FILTER_OFF) {
        proof.kind = "no-filter";
        proof.reason = "none";
        proof.accepted = correctness_ok;
        return proof;
    }
    if (!filter_installed && delta->available && delta->values.jit_rejected > 0 &&
        config->mode == EXECUTOR_JIT) {
        proof.kind = "jit-rejected";
        proof.reason = "jit-rejected";
        return proof;
    }
    if (!correctness_ok) {
        proof.kind = "correctness-fail";
        proof.reason = "correctness-fail";
        return proof;
    }
    if (!delta->available) {
        if (config->mode == EXECUTOR_AUTO && config->stats_state == 0) {
            proof.kind = "linux-active/unsupported-ablation";
            proof.reason = "bpf-stats-unavailable";
            proof.accepted = true;
        }
        return proof;
    }
    if (!delta->valid) {
        proof.kind = "invalid-delta";
        proof.reason = "counter-regression";
        return proof;
    }
    const struct bpf_counters *d = &delta->values;
    uint64_t fallbacks = fallback_total(d);
    if (config->mode == EXECUTOR_INTERPRETER) {
        proof.kind = "verified";
        proof.reason = "none";
        proof.accepted = d->published > 0 && d->native_executed == 0 &&
                         d->interpreter_executed > 0 &&
                         d->fallback_policy_interpreter > 0 &&
                         d->fallback_translation == 0 &&
                         d->fallback_publication == 0 && d->fallback_owner == 0 &&
                         d->fallback_unavailable == 0 && d->jit_rejected == 0;
    } else if (config->mode == EXECUTOR_JIT) {
        proof.kind = "verified";
        proof.reason = "none";
        proof.accepted = d->published > 0 && d->native_executed > 0 &&
                         d->interpreter_executed == 0 && fallbacks == 0 &&
                         d->jit_rejected == 0;
    } else {
        proof.kind = "auto-active";
        proof.reason = "none";
        proof.accepted = d->published > 0 &&
                         (d->native_executed > 0 || d->interpreter_executed > 0);
    }
    if (!proof.accepted) {
        proof.kind = config->mode == EXECUTOR_JIT ? "jit-rejected" : "executor-proof-fail";
        proof.reason = config->mode == EXECUTOR_JIT ? "jit-proof-fail" :
                       "executor-proof-fail";
    }
    return proof;
}

static void emit_delta_fields(const struct bpf_delta *delta) {
    if (!delta->available) {
        printf("published_delta=unsupported native_executed_delta=unsupported "
               "interpreter_executed_delta=unsupported "
               "fallback_policy_interpreter_delta=unsupported "
               "fallback_translation_delta=unsupported "
               "fallback_publication_delta=unsupported fallback_owner_delta=unsupported "
               "fallback_unavailable_delta=unsupported jit_rejected_delta=unsupported "
               "fallback_delta=unsupported");
        return;
    }
    const struct bpf_counters *d = &delta->values;
    printf("published_delta=%" PRIu64 " native_executed_delta=%" PRIu64
           " interpreter_executed_delta=%" PRIu64
           " fallback_policy_interpreter_delta=%" PRIu64
           " fallback_translation_delta=%" PRIu64
           " fallback_publication_delta=%" PRIu64
           " fallback_owner_delta=%" PRIu64
           " fallback_unavailable_delta=%" PRIu64
           " jit_rejected_delta=%" PRIu64 " fallback_delta=%" PRIu64,
           d->published, d->native_executed, d->interpreter_executed,
           d->fallback_policy_interpreter, d->fallback_translation,
           d->fallback_publication, d->fallback_owner, d->fallback_unavailable,
           d->jit_rejected, fallback_total(d));
}

static uint64_t now_ns(clockid_t clock_id) {
    struct timespec ts;
    if (clock_gettime(clock_id, &ts) != 0) {
        error_line("clock_gettime", "failed");
        exit(EXIT_FAILURE);
    }
    return (uint64_t)ts.tv_sec * UINT64_C(1000000000) + (uint64_t)ts.tv_nsec;
}

static uint64_t elapsed_ns(uint64_t start, uint64_t end) {
    return end >= start ? end - start : 0;
}

static void put16(unsigned char *where, uint16_t value) {
    where[0] = (unsigned char)value;
    where[1] = (unsigned char)(value >> 8);
}

static void put32(unsigned char *where, uint32_t value) {
    for (unsigned int index = 0; index < 4; ++index) {
        where[index] = (unsigned char)(value >> (index * 8));
    }
}

static void put64(unsigned char *where, uint64_t value) {
    for (unsigned int index = 0; index < 8; ++index) {
        where[index] = (unsigned char)(value >> (index * 8));
    }
}

static uint16_t get16(const unsigned char *where) {
    return (uint16_t)where[0] | ((uint16_t)where[1] << 8);
}

static uint32_t get32(const unsigned char *where) {
    uint32_t value = 0;
    for (unsigned int index = 0; index < 4; ++index) {
        value |= (uint32_t)where[index] << (index * 8);
    }
    return value;
}

static uint64_t get64(const unsigned char *where) {
    uint64_t value = 0;
    for (unsigned int index = 0; index < 8; ++index) {
        value |= (uint64_t)where[index] << (index * 8);
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
    hash = fnv1a(wire, 32, hash);
    return fnv1a(wire + WIRE_HEADER_LEN, payload_len, hash);
}

static const char *filter_name(enum filter_mode mode) {
    switch (mode) {
    case FILTER_OFF:
        return "filter-off";
    case FILTER_SHORT_ACCEPT:
        return "short-accept";
    case FILTER_BRANCH_HALF:
        return "branch-select-half";
    }
    return "unknown";
}

static const char *topology_name(const struct config *config) {
    return config->formal ? "formal" : "selftest";
}

static unsigned int expected_for(enum filter_mode mode, unsigned int offered) {
    return mode == FILTER_BRANCH_HALF ? (offered + 1U) / 2U : offered;
}

static bool parse_uint(const char *text, unsigned int *value, unsigned int max,
                       const char *option) {
    char *end = NULL;
    unsigned long parsed;
    errno = 0;
    parsed = strtoul(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || parsed > max) {
        fprintf(stderr, "invalid %s: %s\n", option, text);
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
    size_t length = strlen(text);
    uint64_t result = 0;
    if (length != 16) {
        return false;
    }
    for (size_t index = 0; index < length; ++index) {
        int digit = hex_digit(text[index]);
        if (digit < 0) {
            return false;
        }
        result = (result << 4) | (uint64_t)digit;
    }
    *value = result;
    return true;
}

static bool parse_mac(const char *text, unsigned char mac[ETH_ALEN]) {
    unsigned int byte[ETH_ALEN];
    char tail;
    int count = sscanf(text, "%x:%x:%x:%x:%x:%x%c", &byte[0], &byte[1],
                       &byte[2], &byte[3], &byte[4], &byte[5], &tail);
    if (count != ETH_ALEN) {
        return false;
    }
    for (unsigned int index = 0; index < ETH_ALEN; ++index) {
        if (byte[index] > 0xffU) {
            return false;
        }
        mac[index] = (unsigned char)byte[index];
    }
    return true;
}

static bool parse_filter(const char *text, enum filter_mode *mode) {
    if (strcmp(text, "off") == 0 || strcmp(text, "filter-off") == 0) {
        *mode = FILTER_OFF;
        return true;
    }
    if (strcmp(text, "short-accept") == 0) {
        *mode = FILTER_SHORT_ACCEPT;
        return true;
    }
    if (strcmp(text, "branch-select-half") == 0 ||
        strcmp(text, "branch-select(half)") == 0) {
        *mode = FILTER_BRANCH_HALF;
        return true;
    }
    return false;
}

static bool parse_args(int argc, char **argv, struct config *config) {
    memset(config, 0, sizeof(*config));
    config->payload_len = DEFAULT_PAYLOAD;
    config->stream_packets = DEFAULT_STREAM_PACKETS;
    config->warmup = DEFAULT_WARMUP;
    config->latency_samples = DEFAULT_LATENCY_SAMPLES;
    config->timeout_ms = DEFAULT_TIMEOUT_MS;
    memset(config->peer_mac, 0xff, sizeof(config->peer_mac));
    for (int index = 1; index < argc; ++index) {
        const char *option = argv[index];
        const char *value = NULL;
        if (strcmp(option, "--help") == 0 || strcmp(option, "-h") == 0) {
            usage(stdout);
            exit(EXIT_SUCCESS);
        }
        if (strcmp(option, "--formal") == 0) {
            if (config->mode_given && !config->formal) {
                fprintf(stderr, "choose exactly one of --formal and --selftest\n");
                return false;
            }
            config->formal = true;
            config->mode_given = true;
            continue;
        }
        if (strcmp(option, "--selftest") == 0) {
            if (config->mode_given && config->formal) {
                fprintf(stderr, "choose exactly one of --formal and --selftest\n");
                return false;
            }
            config->formal = false;
            config->mode_given = true;
            continue;
        }
        if (strncmp(option, "--interface=", 12) == 0) {
            value = option + 12;
        } else if (strcmp(option, "--interface") == 0 && index + 1 < argc) {
            value = argv[++index];
        }
        if (value != NULL) {
            if (*value == '\0' || strlen(value) >= IFNAMSIZ) {
                fprintf(stderr, "invalid --interface: %s\n", value);
                return false;
            }
            config->interface_name = value;
            config->interface_given = true;
            continue;
        }
        if (strncmp(option, "--filter=", 9) == 0) {
            value = option + 9;
        } else if (strcmp(option, "--filter") == 0 && index + 1 < argc) {
            value = argv[++index];
        }
        if (value != NULL) {
            if (!parse_filter(value, &config->selected_filter)) {
                fprintf(stderr, "invalid --filter: %s\n", value);
                return false;
            }
            config->filter_given = true;
            continue;
        }
        if (strncmp(option, "--peer-mac=", 11) == 0) {
            value = option + 11;
        } else if (strcmp(option, "--peer-mac") == 0 && index + 1 < argc) {
            value = argv[++index];
        }
        if (value != NULL) {
            if (!parse_mac(value, config->peer_mac)) {
                fprintf(stderr, "invalid --peer-mac: %s\n", value);
                return false;
            }
            config->peer_given = true;
            continue;
        }
        if (strncmp(option, "--frame-length=", 15) == 0) {
            value = option + 15;
        } else if (strcmp(option, "--frame-length") == 0 && index + 1 < argc) {
            value = argv[++index];
        } else if (strncmp(option, "--payload-length=", 17) == 0) {
            value = option + 17;
        } else if (strcmp(option, "--payload-length") == 0 && index + 1 < argc) {
            value = argv[++index];
        }
        if (value != NULL) {
            if (!parse_uint(value, &config->payload_len, MAX_PAYLOAD,
                            "--frame-length") || config->payload_len == 0) {
                return false;
            }
            continue;
        }
        if (strncmp(option, "--stream-packets=", 17) == 0) {
            value = option + 17;
        } else if (strcmp(option, "--stream-packets") == 0 && index + 1 < argc) {
            value = argv[++index];
        }
        if (value != NULL) {
            if (!parse_uint(value, &config->stream_packets,
                            MAX_STREAM_PACKETS, "--stream-packets") ||
                config->stream_packets == 0) {
                return false;
            }
            continue;
        }
        if (strncmp(option, "--warmup=", 9) == 0) {
            value = option + 9;
        } else if (strcmp(option, "--warmup") == 0 && index + 1 < argc) {
            value = argv[++index];
        }
        if (value != NULL) {
            if (!parse_uint(value, &config->warmup, MAX_LATENCY_SAMPLES,
                            "--warmup")) {
                return false;
            }
            continue;
        }
        if (strncmp(option, "--latency-samples=", 18) == 0) {
            value = option + 18;
        } else if (strcmp(option, "--latency-samples") == 0 && index + 1 < argc) {
            value = argv[++index];
        }
        if (value != NULL) {
            if (!parse_uint(value, &config->latency_samples,
                            MAX_LATENCY_SAMPLES, "--latency-samples") ||
                config->latency_samples == 0) {
                return false;
            }
            continue;
        }
        if (strncmp(option, "--timeout-ms=", 13) == 0) {
            value = option + 13;
        } else if (strcmp(option, "--timeout-ms") == 0 && index + 1 < argc) {
            value = argv[++index];
        }
        if (value != NULL) {
            if (!parse_uint(value, &config->timeout_ms, 60000U,
                            "--timeout-ms") || config->timeout_ms == 0) {
                return false;
            }
            continue;
        }
        if (strncmp(option, "--run-id=", 9) == 0) {
            value = option + 9;
        } else if (strcmp(option, "--run-id") == 0 && index + 1 < argc) {
            value = argv[++index];
        }
        if (value != NULL) {
            if (!parse_hex64(value, &config->run_id)) {
                fprintf(stderr, "invalid --run-id: %s\n", value);
                return false;
            }
            config->run_id_given = true;
            continue;
        }
        if (strncmp(option, "--executor=", 11) == 0) {
            value = option + 11;
        } else if (strcmp(option, "--executor") == 0 && index + 1 < argc) {
            value = argv[++index];
        }
        if (value != NULL) {
            if (!parse_executor(value, &config->executor)) {
                fprintf(stderr, "invalid --executor: %s\n", value);
                return false;
            }
            config->executor_given = true;
            continue;
        }
        fprintf(stderr, "unknown option: %s\n", option);
        return false;
    }
    if (!config->mode_given) {
        fprintf(stderr, "select --formal for a benchmark NIC or --selftest for loopback\n");
        return false;
    }
    if (config->formal && !config->interface_given) {
        fprintf(stderr, "--formal requires --interface and a host peer\n");
        return false;
    }
    return true;
}

static bool query_interface(const char *name, struct interface_info *info) {
    int fd;
    struct ifreq request;
    memset(info, 0, sizeof(*info));
    if (strlen(name) >= sizeof(info->name)) {
        errno = ENAMETOOLONG;
        return false;
    }
    memcpy(info->name, name, strlen(name) + 1);
    info->ifindex = if_nametoindex(name);
    if (info->ifindex == 0) {
        errno = ENODEV;
        return false;
    }
    info->loopback = strcmp(name, "lo") == 0;
    fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) {
        return false;
    }
    memset(&request, 0, sizeof(request));
    memcpy(request.ifr_name, name, strlen(name) + 1);
    if (ioctl(fd, SIOCGIFHWADDR, &request) != 0) {
        int saved = errno;
        close(fd);
        errno = saved;
        return false;
    }
    memcpy(info->mac, request.ifr_hwaddr.sa_data, ETH_ALEN);
    if ((unsigned short)request.ifr_hwaddr.sa_family == ARPHRD_LOOPBACK) {
        info->loopback = true;
    }
    if (close(fd) != 0) {
        return false;
    }
    return true;
}

static int packet_socket(unsigned int ifindex, bool nonblocking) {
    int type = SOCK_RAW | (nonblocking ? SOCK_NONBLOCK : 0);
    int fd = socket(AF_PACKET, type, htons((uint16_t)PACKET_PROTOCOL));
    struct sockaddr_ll address;
    if (fd < 0) {
        return -1;
    }
    memset(&address, 0, sizeof(address));
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons((uint16_t)PACKET_PROTOCOL);
    address.sll_ifindex = (int)ifindex;
    if (bind(fd, (const struct sockaddr *)&address, sizeof(address)) != 0) {
        int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

static bool attach_filter(int fd, enum filter_mode mode) {
    struct sock_filter short_program[] = {
        BPF_STMT(BPF_LD | BPF_H | BPF_ABS, 12),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, PACKET_PROTOCOL, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, UINT32_MAX),
        BPF_STMT(BPF_RET | BPF_K, 0),
    };
    struct sock_filter branch_program[] = {
        BPF_STMT(BPF_LD | BPF_H | BPF_ABS, 12),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, PACKET_PROTOCOL, 0, 3),
        /* The sequence is little-endian; its low byte is enough for parity. */
        BPF_STMT(BPF_LD | BPF_B | BPF_ABS, ETH_HLEN + 24),
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, 1, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, 0),
        BPF_STMT(BPF_RET | BPF_K, UINT32_MAX),
    };
    struct sock_fprog program;
    if (mode == FILTER_OFF) {
        return true;
    }
    if (mode == FILTER_SHORT_ACCEPT) {
        program.filter = short_program;
        program.len = (unsigned short)(sizeof(short_program) /
                                        sizeof(short_program[0]));
    } else {
        program.filter = branch_program;
        program.len = (unsigned short)(sizeof(branch_program) /
                                       sizeof(branch_program[0]));
    }
    return setsockopt(fd, SOL_SOCKET, SO_ATTACH_FILTER, &program,
                      sizeof(program)) == 0;
}

static void fill_payload(unsigned char *payload, unsigned int length,
                         uint32_t sequence) {
    for (unsigned int index = 0; index < length; ++index) {
        payload[index] = (unsigned char)((sequence + index * 17U) & 0xffU);
    }
}

static size_t make_frame(unsigned char *frame, size_t capacity,
                         const struct config *config,
                         const struct interface_info *info,
                         uint64_t run_id, uint32_t sequence, uint32_t flags,
                         const unsigned char *destination,
                         unsigned int payload_len) {
    size_t wire_len = WIRE_HEADER_LEN + payload_len;
    size_t frame_len = ETH_HLEN + wire_len;
    unsigned char *wire;
    (void)config;
    if (frame_len > capacity || frame_len > MAX_FRAME) {
        return 0;
    }
    memset(frame, 0, frame_len < 60U ? 60U : frame_len);
    memcpy(frame, destination, ETH_ALEN);
    memcpy(frame + ETH_ALEN, info->mac, ETH_ALEN);
    frame[12] = (unsigned char)(PACKET_PROTOCOL >> 8);
    frame[13] = (unsigned char)PACKET_PROTOCOL;
    wire = frame + ETH_HLEN;
    memcpy(wire, "TKPFNET1", 8);
    put16(wire + 8, WIRE_VERSION);
    put16(wire + 10, WIRE_HEADER_LEN);
    put32(wire + 12, flags);
    put64(wire + 16, run_id);
    put32(wire + 24, sequence);
    put32(wire + 28, payload_len);
    fill_payload(wire + WIRE_HEADER_LEN, payload_len, sequence);
    put32(wire + 32, wire_checksum(wire, payload_len));
    return frame_len < 60U ? 60U : frame_len;
}

static int parse_frame(const unsigned char *frame, size_t frame_len,
                       struct parsed_packet *packet) {
    const unsigned char *wire;
    size_t payload_len;
    uint32_t expected;
    uint32_t actual;
    if (frame_len < ETH_HLEN + WIRE_HEADER_LEN ||
        ((unsigned int)frame[12] << 8 | frame[13]) != PACKET_PROTOCOL) {
        return -1;
    }
    wire = frame + ETH_HLEN;
    if (memcmp(wire, "TKPFNET1", 8) != 0 || get16(wire + 8) != WIRE_VERSION ||
        get16(wire + 10) != WIRE_HEADER_LEN) {
        return -1;
    }
    payload_len = get32(wire + 28);
    if (payload_len > MAX_PAYLOAD ||
        frame_len < ETH_HLEN + WIRE_HEADER_LEN + payload_len) {
        return -1;
    }
    expected = get32(wire + 32);
    actual = wire_checksum(wire, payload_len);
    packet->run_id = get64(wire + 16);
    packet->seq = get32(wire + 24);
    packet->payload_len = (uint32_t)payload_len;
    packet->flags = get32(wire + 12);
    if (expected != actual) {
        return -2;
    }
    return 0;
}

static bool poll_fd(int fd, short events, unsigned int timeout_ms) {
    struct pollfd descriptor = {.fd = fd, .events = events, .revents = 0};
    int result;
    do {
        result = poll(&descriptor, 1, (int)timeout_ms);
    } while (result < 0 && errno == EINTR);
    return result > 0 && (descriptor.revents & events) != 0;
}

static bool send_frame(int fd, unsigned int ifindex,
                       const unsigned char destination[ETH_ALEN],
                       const unsigned char *frame, size_t frame_len,
                       unsigned int timeout_ms) {
    struct sockaddr_ll address;
    uint64_t deadline = now_ns(CLOCK_MONOTONIC) +
                        (uint64_t)timeout_ms * UINT64_C(1000000);
    memset(&address, 0, sizeof(address));
    address.sll_family = AF_PACKET;
    address.sll_ifindex = (int)ifindex;
    address.sll_halen = ETH_ALEN;
    address.sll_protocol = htons((uint16_t)PACKET_PROTOCOL);
    memcpy(address.sll_addr, destination, ETH_ALEN);
    for (;;) {
        ssize_t result = sendto(fd, frame, frame_len, 0,
                                (const struct sockaddr *)&address,
                                sizeof(address));
        if (result == (ssize_t)frame_len) {
            return true;
        }
        if (result < 0 && errno != EINTR && errno != EAGAIN && errno != EWOULDBLOCK) {
            return false;
        }
        if (now_ns(CLOCK_MONOTONIC) >= deadline ||
            !poll_fd(fd, POLLOUT, 10U)) {
            errno = ETIMEDOUT;
            return false;
        }
    }
}

static bool send_wire(int tx, const struct config *config,
                      const struct interface_info *info,
                      uint64_t run_id, uint32_t sequence, uint32_t flags,
                      const unsigned char destination[ETH_ALEN],
                      unsigned int payload_len) {
    unsigned char frame[MAX_FRAME];
    size_t length = make_frame(frame, sizeof(frame), config, info, run_id,
                               sequence, flags, destination, payload_len);
    return length != 0 && send_frame(tx, info->ifindex, destination, frame,
                                     length, config->timeout_ms);
}

static bool send_hello(int tx, const struct config *config,
                       const struct interface_info *info,
                       uint64_t run_id, enum filter_mode mode,
                       uint32_t stream_base,
                       const unsigned char destination[ETH_ALEN]) {
    unsigned char frame[MAX_FRAME];
    size_t length = make_frame(frame, sizeof(frame), config, info, run_id,
                               stream_base | 1U, FLAG_HELLO, destination,
                               HELLO_LEN);
    unsigned char *payload = frame + ETH_HLEN + WIRE_HEADER_LEN;
    if (length == 0) {
        return false;
    }
    put32(payload, HELLO_MAGIC);
    put32(payload + 4, (uint32_t)mode);
    put32(payload + 8, config->stream_packets);
    put32(payload + 12, config->payload_len);
    put32(payload + 16, stream_base);
    put32(payload + 20, config->latency_samples);
    put32(frame + ETH_HLEN + 32,
          wire_checksum(frame + ETH_HLEN, HELLO_LEN));
    return send_frame(tx, info->ifindex, destination, frame, length,
                       config->timeout_ms);
}

static bool receive_one(int rx, unsigned char frame[MAX_FRAME], size_t *length,
                        unsigned int timeout_ms) {
    if (!poll_fd(rx, POLLIN, timeout_ms)) {
        errno = ETIMEDOUT;
        return false;
    }
    for (;;) {
        ssize_t result = recv(rx, frame, MAX_FRAME, 0);
        if (result >= 0) {
            *length = (size_t)result;
            return true;
        }
        if (errno == EINTR) {
            continue;
        }
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            return false;
        }
        return false;
    }
}

static bool sequence_in_range(uint32_t sequence, uint32_t base,
                              unsigned int offered) {
    uint64_t end = (uint64_t)base + offered;
    return (uint64_t)sequence >= base && (uint64_t)sequence < end;
}

static bool collect_stream(int rx, const struct config *config,
                           enum filter_mode mode, uint64_t run_id,
                           uint32_t base, uint64_t start_ns,
                           struct stream_stats *stats) {
    unsigned char frame[MAX_FRAME];
    unsigned char *seen = calloc(stats->offered, sizeof(*seen));
    uint64_t deadline = start_ns +
                        (uint64_t)config->timeout_ms * UINT64_C(1000000);
    if (seen == NULL) {
        errno = ENOMEM;
        return false;
    }
    while (stats->accepted < stats->expected && now_ns(CLOCK_MONOTONIC) < deadline) {
        size_t length = 0;
        uint64_t remaining = deadline - now_ns(CLOCK_MONOTONIC);
        unsigned int wait_ms = (unsigned int)((remaining + 999999U) / 1000000U);
        struct parsed_packet packet;
        int parsed;
        if (wait_ms == 0) {
            wait_ms = 1;
        }
        if (wait_ms > 50U) {
            wait_ms = 50U;
        }
        if (!receive_one(rx, frame, &length, wait_ms)) {
            continue;
        }
        parsed = parse_frame(frame, length, &packet);
        if (parsed == -2) {
            stats->checksum++;
            stats->rejected++;
            continue;
        }
        if (parsed != 0 || packet.run_id != run_id ||
            packet.flags != FLAG_STREAM ||
            packet.payload_len != config->payload_len ||
            !sequence_in_range(packet.seq, base, stats->offered)) {
            if (parsed == 0 && packet.run_id != run_id) {
                continue;
            }
            stats->rejected++;
            continue;
        }
        if (mode == FILTER_BRANCH_HALF && (packet.seq & 1U) == 0) {
            stats->rejected++;
            continue;
        }
        unsigned int offset = packet.seq - base;
        if (seen[offset] != 0) {
            stats->duplicate++;
            continue;
        }
        seen[offset] = 1;
        stats->accepted++;
        stats->bytes += packet.payload_len;
    }
    stats->elapsed_ns = elapsed_ns(start_ns, now_ns(CLOCK_MONOTONIC));
    if (stats->elapsed_ns == 0) {
        stats->elapsed_ns = 1;
    }
    stats->missing = stats->offered - stats->accepted;
    free(seen);
    return stats->accepted == stats->expected;
}

static bool wait_echo(int rx, const struct config *config, enum filter_mode mode,
                      uint64_t run_id, uint32_t sequence, uint32_t expected_flags,
                      uint64_t *wall, uint64_t *cpu) {
    unsigned char frame[MAX_FRAME];
    uint64_t wall_start = now_ns(CLOCK_MONOTONIC);
    uint64_t cpu_start = now_ns(CLOCK_PROCESS_CPUTIME_ID);
    uint64_t deadline = wall_start +
                        (uint64_t)config->timeout_ms * UINT64_C(1000000);
    (void)mode;
    for (;;) {
        size_t length = 0;
        uint64_t current = now_ns(CLOCK_MONOTONIC);
        unsigned int wait_ms;
        struct parsed_packet packet;
        int parsed;
        if (current >= deadline) {
            errno = ETIMEDOUT;
            return false;
        }
        wait_ms = (unsigned int)(((deadline - current) + 999999U) / 1000000U);
        if (wait_ms == 0) {
            wait_ms = 1;
        }
        if (!receive_one(rx, frame, &length, wait_ms)) {
            continue;
        }
        parsed = parse_frame(frame, length, &packet);
        if (parsed != 0 || packet.run_id != run_id || packet.seq != sequence ||
            packet.flags != expected_flags ||
            packet.payload_len != config->payload_len) {
            continue;
        }
        *wall = elapsed_ns(wall_start, now_ns(CLOCK_MONOTONIC));
        *cpu = elapsed_ns(cpu_start, now_ns(CLOCK_PROCESS_CPUTIME_ID));
        return true;
    }
}

static bool run_latency_window(int rx, int tx, const struct config *config,
                               const struct interface_info *info,
                               enum filter_mode mode, uint64_t run_id,
                               uint32_t stream_base,
                               const unsigned char destination[ETH_ALEN],
                               struct latency_stats *latency) {
    uint32_t sequence = stream_base + config->stream_packets + 1001U;
    unsigned int total = config->warmup + config->latency_samples;
    if (mode == FILTER_BRANCH_HALF) {
        sequence |= 1U;
    }
    for (unsigned int index = 0; index < total; ++index) {
        uint64_t wall;
        uint64_t cpu;
        if (!send_wire(tx, config, info, run_id, sequence,
                       config->formal ? FLAG_ECHO_REQUEST : FLAG_ECHO_REQUEST,
                       destination, config->payload_len) ||
            !wait_echo(rx, config, mode, run_id, sequence,
                       config->formal ? FLAG_ECHO_RESPONSE : FLAG_ECHO_REQUEST,
                       &wall, &cpu)) {
            return false;
        }
        if (index >= config->warmup && latency->count < MAX_LATENCY_SAMPLES) {
            latency->wall[latency->count] = wall;
            latency->cpu[latency->count] = cpu;
            latency->count++;
        }
        sequence += mode == FILTER_BRANCH_HALF ? 2U : 1U;
    }
    return latency->count == config->latency_samples;
}

static uint64_t quantile(const uint64_t *values, unsigned int count,
                         unsigned int permille) {
    uint64_t *copy = malloc((size_t)count * sizeof(*copy));
    uint64_t result;
    if (copy == NULL) {
        errno = ENOMEM;
        return 0;
    }
    memcpy(copy, values, (size_t)count * sizeof(*copy));
    for (unsigned int left = 0; left < count; ++left) {
        unsigned int smallest = left;
        for (unsigned int right = left + 1; right < count; ++right) {
            if (copy[right] < copy[smallest]) {
                smallest = right;
            }
        }
        uint64_t temporary = copy[left];
        copy[left] = copy[smallest];
        copy[smallest] = temporary;
    }
    unsigned int rank = ((count * permille) + 999U) / 1000U;
    if (rank == 0) {
        rank = 1;
    }
    if (rank > count) {
        rank = count;
    }
    result = copy[rank - 1];
    free(copy);
    return result;
}

static void emit_run(uint64_t run_id, unsigned int cells,
                     const struct config *config) {
    printf(PERF_RUN " schema=%s workload=packet run_id=%016llx cells=%u "
           "sizes=%u qd=1 ops=stream-echo clocks=monotonic,process-cpu "
           "executor=%s domain=packet topology=%s\n",
           PERF_SCHEMA, (unsigned long long)run_id, cells,
           config->payload_len, executor_name(config->executor),
           topology_name(config));
    fflush(stdout);
}

static void emit_stats(const struct config *config, enum filter_mode mode,
                       uint64_t run_id, const struct stream_stats *stats) {
    uint64_t elapsed = stats->elapsed_ns == 0 ? 1 : stats->elapsed_ns;
    unsigned long long pps = (unsigned long long)
        ((stats->accepted * UINT64_C(1000000000)) / elapsed);
    printf("PACKET_PERF_STATS schema=%s topology=%s interface=%s run_id=%016llx "
           "mode=%s offered=%u accepted=%u rejected=%u missing=%u duplicate=%u "
           "checksum=%u bytes=%llu pps=%llu loss=%u oracle=accept-half\n",
           PERF_SCHEMA, topology_name(config), config->interface_name,
           (unsigned long long)run_id, filter_name(mode), stats->offered,
           stats->accepted, stats->rejected, stats->missing, stats->duplicate,
           stats->checksum, (unsigned long long)stats->bytes, pps,
           stats->missing);
    fflush(stdout);
}

static void emit_correctness(const struct config *config, enum filter_mode mode,
                             uint64_t run_id, const struct stream_stats *stats,
                             const struct cell_proof *proof,
                             const char *status, const char *reason) {
    unsigned int marker_missing = strcmp(status, "ok") == 0 ? 0U : stats->missing;
    printf(PERF_CORRECTNESS " schema=%s workload=packet run_id=%016llx "
           "cell=packet-%s op=stream-echo size=%u qd=1 status=%s "
           "reason=%s calls=%u missing=%u duplicate=%u checksum=%u "
           "executor=%s domain=packet proof=%s oracle=accept-half ",
           PERF_SCHEMA, (unsigned long long)run_id, filter_name(mode),
           config->payload_len, status, reason == NULL ? "none" : reason,
           stats->accepted, marker_missing, stats->duplicate, stats->checksum,
           executor_name(config->executor), proof->kind);
    emit_delta_fields(&proof->delta);
    printf(" topology=%s mode=%s\n", topology_name(config), filter_name(mode));
    fflush(stdout);
}

static void emit_window_and_latency(const struct config *config,
                                    enum filter_mode mode, uint64_t run_id,
                                    const struct latency_stats *latency,
                                    bool success) {
    unsigned int count = success ? latency->count : 0U;
    uint64_t wall_p50 = count == 0 ? 0 : quantile(latency->wall, count, 500);
    uint64_t wall_p99 = count == 0 ? 0 : quantile(latency->wall, count, 990);
    uint64_t cpu_p50 = count == 0 ? 0 : quantile(latency->cpu, count, 500);
    uint64_t cpu_p99 = count == 0 ? 0 : quantile(latency->cpu, count, 990);
    const char *status = success ? "ok" : "fail";
    printf(PERF_WINDOW " schema=%s workload=packet run_id=%016llx "
           "cell=packet-%s op=stream-echo size=%u qd=1 status=%s warmup=%u "
           "samples=%u clocks=monotonic,process-cpu topology=%s mode=%s "
           "executor=%s domain=packet\n",
           PERF_SCHEMA, (unsigned long long)run_id, filter_name(mode),
           config->payload_len, status, config->warmup, count,
           topology_name(config), filter_name(mode), executor_name(config->executor));
    printf(PERF_LATENCY " schema=%s workload=packet run_id=%016llx "
           "cell=packet-%s op=stream-echo size=%u qd=1 status=%s samples=%u "
           "wall_p50_ns=%llu wall_p99_ns=%llu cpu_p50_ns=%llu cpu_p99_ns=%llu "
           "sink=%s topology=%s mode=%s executor=%s domain=packet\n",
           PERF_SCHEMA, (unsigned long long)run_id, filter_name(mode),
           config->payload_len, status, count,
           (unsigned long long)wall_p50, (unsigned long long)wall_p99,
           (unsigned long long)cpu_p50, (unsigned long long)cpu_p99,
           config->formal ? "host-peer-closed-loop" : "selftest-echo",
           topology_name(config), filter_name(mode), executor_name(config->executor));
    fflush(stdout);
}

static int run_mode(const struct config *config,
                    const struct interface_info *info,
                    enum filter_mode mode, uint64_t run_id,
                    uint32_t stream_base, const unsigned char destination[ETH_ALEN],
                    const char **failure_reason) {
    int rx = -1;
    int tx = -1;
    struct stream_stats stats;
    struct latency_stats latency;
    struct bpf_counters before;
    struct bpf_counters after;
    struct bpf_delta delta = {.available = false, .valid = false};
    struct cell_proof proof = {.delta = delta, .kind = "unsupported-ablation",
                               .reason = "bpf-stats-unavailable", .accepted = false};
    bool stream_ok;
    bool latency_ok;
    bool correctness_ok;
    bool filter_installed = false;
    int before_state;
    *failure_reason = NULL;
    stream_ok = true;
    memset(&stats, 0, sizeof(stats));
    memset(&latency, 0, sizeof(latency));
    stats.offered = config->stream_packets;
    stats.expected = expected_for(mode, stats.offered);
    int control_state = set_executor_control("packet", config->executor);
    if (control_state < 0 ||
        (config->executor != EXECUTOR_AUTO && control_state != 1)) {
        *failure_reason = "bpf-control-unavailable";
        proof.kind = "unsupported-ablation";
        proof.reason = *failure_reason;
        emit_correctness(config, mode, run_id, &stats, &proof, "unsupported",
                         *failure_reason);
        emit_stats(config, mode, run_id, &stats);
        return 2;
    }
    before_state = read_bpf_stats("packet", &before);
    if (before_state < 0 ||
        (config->executor != EXECUTOR_AUTO && before_state != 1)) {
        *failure_reason = "bpf-stats-unavailable";
        proof.kind = "unsupported-ablation";
        proof.reason = *failure_reason;
        emit_correctness(config, mode, run_id, &stats, &proof, "unsupported",
                         *failure_reason);
        emit_stats(config, mode, run_id, &stats);
        return 2;
    }
    rx = packet_socket(info->ifindex, true);
    tx = packet_socket(info->ifindex, true);
    if (rx < 0 || tx < 0) {
        error_line("packet-socket", "open-or-filter");
        if (rx >= 0) {
            close(rx);
        }
        if (tx >= 0) {
            close(tx);
        }
        *failure_reason = "packet-socket-unavailable";
        proof.kind = "unsupported-ablation";
        proof.reason = *failure_reason;
        emit_correctness(config, mode, run_id, &stats, &proof, "unsupported",
                         *failure_reason);
        emit_stats(config, mode, run_id, &stats);
        return 2;
    }
    if (mode != FILTER_OFF) {
        filter_installed = attach_filter(rx, mode);
        if (!filter_installed) {
            error_line("packet-filter", "attach-failed");
        }
    }
    if (!filter_installed && mode != FILTER_OFF) {
        stream_ok = false;
        *failure_reason = config->executor == EXECUTOR_JIT ? "jit-rejected" :
                          "packet-filter-unavailable";
    } else if (config->formal) {
        uint64_t stream_start = now_ns(CLOCK_MONOTONIC);
        if (!send_hello(tx, config, info, run_id, mode, stream_base, destination)) {
            stream_ok = false;
            *failure_reason = "host-peer-unavailable";
        } else {
            stream_ok = collect_stream(rx, config, mode, run_id, stream_base,
                                       stream_start, &stats);
            *failure_reason = stream_ok ? NULL :
                (stats.accepted == 0 ? "host-peer-unavailable" : "stream-incomplete");
        }
    } else {
        uint64_t stream_start = now_ns(CLOCK_MONOTONIC);
        for (unsigned int index = 0; index < config->stream_packets; ++index) {
            if (!send_wire(tx, config, info, run_id, stream_base + index,
                           FLAG_STREAM, destination, config->payload_len)) {
                stream_ok = false;
                *failure_reason = "stream-send-failed";
                break;
            }
        }
        if (!stream_ok && *failure_reason == NULL) {
            *failure_reason = "stream-send-failed";
        }
        if (*failure_reason == NULL || strcmp(*failure_reason, "stream-send-failed") != 0) {
            stream_ok = collect_stream(rx, config, mode, run_id, stream_base,
                                       stream_start, &stats);
            *failure_reason = stream_ok ? NULL : "stream-oracle-mismatch";
        }
    }
    correctness_ok = stream_ok && stats.duplicate == 0 && stats.checksum == 0 &&
                     stats.rejected == 0;
    latency_ok = false;
    if (correctness_ok) {
        latency_ok = run_latency_window(rx, tx, config, info, mode, run_id,
                                        stream_base, destination, &latency);
        if (!latency_ok) {
            *failure_reason = config->formal ? "host-peer-unavailable" :
                              "selftest-echo-failed";
        }
    }
    int after_state = read_bpf_stats("packet", &after);
    if (before_state == 1 && after_state == 1) {
        (void)make_delta(&before, &after, &delta);
    }
    proof = evaluate_proof(mode, &(struct executor_config){
                                  .mode = config->executor,
                                  .control_state = control_state,
                                  .stats_state = before_state,
                              }, &delta, correctness_ok, filter_installed);
    bool unsupported = !correctness_ok && config->formal && stats.accepted == 0;
    if (!proof.accepted && config->executor != EXECUTOR_AUTO &&
        (filter_installed || mode == FILTER_OFF)) {
        unsupported = true;
    }
    const char *status = proof.accepted && correctness_ok ? "ok" :
        (unsupported ? "unsupported" : "fail");
    if (proof.accepted && correctness_ok) {
        *failure_reason = latency_ok ? NULL : *failure_reason;
    } else if (*failure_reason == NULL) {
        *failure_reason = proof.reason;
    }
    close(rx);
    close(tx);
    emit_correctness(config, mode, run_id, &stats, &proof, status,
                     status[0] == 'o' ? "none" : *failure_reason);
    emit_stats(config, mode, run_id, &stats);
    if (proof.accepted && correctness_ok) {
        emit_window_and_latency(config, mode, run_id, &latency, latency_ok);
    }
    if (strcmp(status, "unsupported") == 0) {
        return 2;
    }
    if (strcmp(status, "fail") == 0 || !latency_ok) {
        return 1;
    }
    return 0;
}

int main(int argc, char **argv) {
    struct config config;
    struct interface_info info;
    unsigned char destination[ETH_ALEN];
    enum filter_mode modes[3];
    unsigned int mode_count;
    uint64_t run_id;
    uint32_t sequence_base = 1000U;
    unsigned int unsupported = 0;
    if (!parse_args(argc, argv, &config)) {
        usage(stderr);
        return EXIT_FAILURE;
    }
    config.interface_name = config.interface_given ? config.interface_name : "lo";
    if (!query_interface(config.interface_name, &info)) {
        error_line("interface", "query-failed");
        return EXIT_FAILURE;
    }
    if (config.formal && info.loopback) {
        fprintf(stderr, "--formal rejects loopback interface %s; use a benchmark NIC\n",
                info.name);
        return EXIT_FAILURE;
    }
    if (config.formal) {
        memcpy(destination, config.peer_mac, ETH_ALEN);
    } else {
        memcpy(destination, info.mac, ETH_ALEN);
    }
    if (config.run_id_given) {
        run_id = config.run_id;
    } else {
        run_id = now_ns(CLOCK_MONOTONIC) ^ ((uint64_t)getpid() << 32);
        run_id ^= UINT64_C(0x544b50464e455431);
    }
    if (config.filter_given) {
        modes[0] = config.selected_filter;
        mode_count = 1;
    } else {
        modes[0] = FILTER_OFF;
        modes[1] = FILTER_SHORT_ACCEPT;
        modes[2] = FILTER_BRANCH_HALF;
        mode_count = 3;
    }
    emit_run(run_id, mode_count, &config);
    struct executor_config executor_state;
    int capability = prepare_executor("packet", config.executor, &executor_state);
    bool run_failed = false;
    bool run_unsupported = capability <= 0;
    if (capability <= 0) {
        for (unsigned int index = 0; index < mode_count; ++index) {
            struct stream_stats stats = {
                .offered = config.stream_packets,
                .expected = expected_for(modes[index], config.stream_packets),
            };
            struct bpf_delta delta = {.available = false, .valid = false};
            struct cell_proof proof = {
                .delta = delta,
                .kind = "unsupported-ablation",
                .reason = capability < 0 ? "bpf-proc-error" :
                          "bpf-control-unavailable",
                .accepted = false,
            };
            emit_correctness(&config, modes[index], run_id, &stats, &proof,
                             "unsupported", proof.reason);
            emit_stats(&config, modes[index], run_id, &stats);
            ++unsupported;
        }
    }
    for (unsigned int index = 0; index < mode_count; ++index) {
        if (capability <= 0) {
            break;
        }
        const char *reason = NULL;
        sequence_base &= ~1U;
        uint32_t mode_base = sequence_base;
        if (modes[index] == FILTER_BRANCH_HALF) {
            mode_base |= 1U;
        }
        int result = run_mode(&config, &info, modes[index], run_id, mode_base,
                              destination, &reason);
        if (result == 2) {
            ++unsupported;
        } else if (result != 0) {
            run_failed = true;
        }
        sequence_base += config.stream_packets + 4096U;
    }
    printf(PERF_DONE " schema=%s workload=packet run_id=%016llx status=%s "
           "cells=%u unsupported=%u executor=%s domain=packet topology=%s proof=%s\n", PERF_SCHEMA,
           (unsigned long long)run_id, run_failed ? "fail" :
           ((run_unsupported || unsupported != 0) ? "unsupported" : "ok"),
           mode_count, unsupported, executor_name(config.executor),
           topology_name(&config), run_failed ? "fail" :
           ((run_unsupported || unsupported != 0) ? "unsupported" : "verified"));
    fflush(stdout);
    return run_failed ? EXIT_FAILURE : EXIT_SUCCESS;
}
