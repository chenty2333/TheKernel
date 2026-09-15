/* Console output integrity under concurrent writers.
 *
 * The ABI differential compares the multiset of console records between a
 * TheKernel guest and a Linux guest.  Every record this program prints must
 * therefore reach the console exactly once, whole, and in a form that is
 * byte-identical on both kernels.  Two kernel defects used to break that:
 * a line written by one process was observed split by another process's
 * bytes, and a line was observed emitted a second time several records later.
 *
 * The program drives both shapes at once and leaves the verdict to the
 * reader of the transcript:
 *
 *   - every writer emits one record per `write(2)` call, so any split inside
 *     a record is a kernel defect and not a userspace buffering artefact;
 *   - the records are self-describing: the payload after the sequence number
 *     is a pseudo-random string derived from that same number, so a record
 *     that lost, gained or exchanged bytes with another record cannot match;
 *   - the concurrent phase prints plain `CONSOLE_INTEGRITY` records, while
 *     the handful of `THEKERNEL_ABI_ASSERT` records that the differential
 *     compares are printed by writers whose output is ordered by the program
 *     (a pipe round trip), so the ABI record multiset stays deterministic.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define CASE "console_integrity.raw-differential"
#define LINES_PER_WRITER 300
#define FORK_ITERATIONS 12
#define FORK_CHILD_DELAY 200000
#define PAYLOAD_GROUPS 8
#define PAYLOAD_GROUP 4

static const char alphabet[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/* xorshift64*, so the host checker can rebuild every payload exactly. */
static uint64_t next_random(uint64_t *state) {
    uint64_t x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    return x * 2685821657736338717ULL;
}

static void payload_for(char side, unsigned sequence, char *out) {
    uint64_t state = 0x9e3779b97f4a7c15ULL ^ ((uint64_t)(unsigned char)side << 40) ^ sequence;
    unsigned at = 0;
    for (int group = 0; group < PAYLOAD_GROUPS; group++) {
        for (int byte = 0; byte < PAYLOAD_GROUP; byte++) {
            out[at++] = alphabet[next_random(&state) % (sizeof(alphabet) - 1)];
        }
        if (group + 1 != PAYLOAD_GROUPS) {
            out[at++] = '-';
        }
    }
    out[at] = '\0';
}

/* One record per call: the kernel console must not split it. */
static int write_all(int fd, const char *bytes, size_t length) {
    size_t done = 0;
    while (done < length) {
        ssize_t written = write(fd, bytes + done, length - done);
        if (written < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (written == 0) {
            return -1;
        }
        done += (size_t)written;
    }
    return 0;
}

static int emit(const char *line) {
    char record[160];
    int length = snprintf(record, sizeof(record), "%s\n", line);
    if (length <= 0 || (size_t)length >= sizeof(record)) {
        return -1;
    }
    return write_all(STDOUT_FILENO, record, (size_t)length);
}

static int integrity_record(char side, unsigned sequence) {
    char payload[PAYLOAD_GROUPS * (PAYLOAD_GROUP + 1)];
    char line[160];
    payload_for(side, sequence, payload);
    if (snprintf(line, sizeof(line), "CONSOLE_INTEGRITY %c %04u %s", side, sequence, payload) <= 0) {
        return -1;
    }
    return emit(line);
}

/* A forked child exits while the parent sits in a blocking syscall, so the
 * parent takes SIGCHLD exactly there.  Linux restarts that syscall and
 * nothing else: the parent's earlier, already-completed console writes must
 * not be replayed, and no record may appear twice. */
static void fail(const char *message);

static volatile sig_atomic_t sigchld_seen;

static void note_sigchld(int signo) {
    (void)signo;
    sigchld_seen = 1;
}

static void restart_phase(void) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = note_sigchld;
    action.sa_flags = SA_RESTART;
    if (sigaction(SIGCHLD, &action, NULL) != 0) {
        fail("sigaction");
    }

    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
        fail("socketpair");
    }

    pid_t signal_child = fork();
    if (signal_child < 0) {
        fail("fork signal child");
    }
    if (signal_child == 0) {
        close(pair[0]);
        close(pair[1]);
        usleep(100000);
        _exit(0);
    }

    pid_t data_child = fork();
    if (data_child < 0) {
        fail("fork data child");
    }
    if (data_child == 0) {
        close(pair[0]);
        usleep(400000);
        if (write_all(pair[1], "z", 1) != 0) {
            _exit(9);
        }
        _exit(0);
    }
    close(pair[1]);

    if (integrity_record('S', 1) != 0 || integrity_record('S', 2) != 0 ||
        integrity_record('S', 3) != 0) {
        fail("restart records");
    }

    char byte = 0;
    ssize_t got;
    do {
        got = recv(pair[0], &byte, 1, 0);
    } while (got < 0 && errno == EINTR);
    if (got != 1 || byte != 'z') {
        fail("restart recv");
    }
    if (integrity_record('S', 4) != 0) {
        fail("restart record");
    }
    if (!sigchld_seen) {
        fail("no SIGCHLD");
    }

    int status = 0;
    if (waitpid(signal_child, &status, 0) != signal_child ||
        waitpid(data_child, &status, 0) != data_child) {
        fail("restart waitpid");
    }
}

static int abi_assert(const char *assertion) {
    char line[160];
    if (snprintf(line, sizeof(line), "THEKERNEL_ABI_ASSERT %s %s pass", CASE, assertion) <= 0) {
        return -1;
    }
    return emit(line);
}

static void fail(const char *message) {
    fprintf(stderr, "console-integrity: %s: %s\n", message, strerror(errno));
    _exit(70);
}

static uint16_t loopback_listener(int *listener) {
    struct sockaddr_in address;
    memset(&address, 0, sizeof(address));
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    *listener = socket(AF_INET, SOCK_STREAM, 0);
    if (*listener < 0 || bind(*listener, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        listen(*listener, 1) != 0) {
        return 0;
    }
    socklen_t length = sizeof(address);
    if (getsockname(*listener, (struct sockaddr *)&address, &length) != 0) {
        return 0;
    }
    return ntohs(address.sin_port);
}

/* A forked child blocks in accept()/read() and then exits while the parent is
 * blocked in recv() waiting for the rest of a fixed-length payload, so the
 * parent takes SIGCHLD exactly at the completion of that receive.  Linux
 * restarts the interrupted receive; the parent's already-completed console
 * records must not be replayed. */
static void fork_blocked_child_phase(void) {
    for (unsigned iteration = 0; iteration < FORK_ITERATIONS; iteration++) {
        int listener = -1;
        uint16_t port = loopback_listener(&listener);
        if (port == 0) {
            fail("listener");
        }

        pid_t child = fork();
        if (child < 0) {
            fail("fork");
        }
        if (child == 0) {
            int peer = accept(listener, NULL, NULL);
            close(listener);
            if (peer < 0) {
                _exit(65);
            }
            if (write_all(peer, "abcd", 4) != 0) {
                _exit(66);
            }
            usleep(FORK_CHILD_DELAY);
            if (write_all(peer, "efghij", 6) != 0) {
                _exit(67);
            }
            close(peer);
            _exit(0);
        }
        if (integrity_record('A', iteration) != 0) {
            fail("fork record");
        }
        close(listener);

        struct sockaddr_in address;
        memset(&address, 0, sizeof(address));
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        address.sin_port = htons(port);
        int client = socket(AF_INET, SOCK_STREAM, 0);
        int connected =
            client >= 0 && connect(client, (struct sockaddr *)&address, sizeof(address)) == 0;
        if (integrity_record('B', iteration) != 0 || !connected) {
            fail("connect");
        }
        if (write_all(client, "x", 1) != 0) {
            fail("client send");
        }

        char payload[16];
        memset(payload, 0, sizeof(payload));
        errno = 0;
        /* No MSG_WAITALL: the baseline kernel does not implement it, and two
         * receives express the same thing -- the first takes the four bytes
         * the child already sent, the second blocks until the child's exit
         * delivers the remaining six. */
        ssize_t count = 0;
        while (count < 10) {
            ssize_t received = recv(client, payload + count, (size_t)(10 - count), 0);
            if (received <= 0) {
                break;
            }
            count += received;
        }
        close(client);
        if (integrity_record('C', iteration) != 0 || count != 10 ||
            memcmp(payload, "abcdefghij", 10) != 0) {
            fail("recv");
        }
        int status = 0;
        if (waitpid(child, &status, 0) != child) {
            fail("waitpid");
        }
        if (integrity_record('D', iteration) != 0 || !WIFEXITED(status) ||
            WEXITSTATUS(status) != 0) {
            fail("child status");
        }
    }
}

/* Both processes write their own records as fast as they can; the gate keeps
 * the two write streams overlapping rather than merely sequential. */
static void concurrent_writer(char side, int gate) {
    if (gate >= 0) {
        char token;
        if (read(gate, &token, 1) != 1) {
            fail("gate read");
        }
    }
    for (unsigned sequence = 0; sequence < LINES_PER_WRITER; sequence++) {
        if (integrity_record(side, sequence) != 0) {
            fail("integrity record");
        }
    }
}

int main(int argc, char **argv) {
    const char *phase = argc > 1 ? argv[1] : "interleave";
    emit("THEKERNEL_ABI_CASE " CASE);

    if (strcmp(phase, "restart") == 0) {
        restart_phase();
        emit("THEKERNEL_CONSOLE_INTEGRITY_OK");
        emit("THEKERNEL_ABI_RESULT " CASE " pass");
        return 0;
    }
    if (strcmp(phase, "fork") == 0) {
        fork_blocked_child_phase();
        emit("THEKERNEL_CONSOLE_INTEGRITY_OK");
        emit("THEKERNEL_ABI_RESULT " CASE " pass");
        return 0;
    }

    int gate[2];
    if (pipe(gate) != 0) {
        fail("pipe");
    }
    pid_t child = fork();
    if (child < 0) {
        fail("fork");
    }
    if (child == 0) {
        close(gate[1]);
        concurrent_writer('C', gate[0]);
        _exit(0);
    }
    close(gate[0]);
    /* Release the child, then write concurrently with it. */
    if (write_all(gate[1], "g", 1) != 0) {
        fail("gate release");
    }
    close(gate[1]);
    concurrent_writer('P', -1);
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        fail("waitpid");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fail("child status");
    }

    if (abi_assert("INTERLEAVED_WRITERS") != 0) {
        fail("assert");
    }
    emit("THEKERNEL_CONSOLE_INTEGRITY_OK");
    emit("THEKERNEL_ABI_RESULT " CASE " pass");
    return 0;
}
