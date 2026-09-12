#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/mman.h>
#include <sys/un.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static void fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_UNIX_WRITE_CREDENTIALS_FAIL %s errno=%d\n", stage, errno);
    exit(1);
}
static void mark(const char *assertion)
{
    printf("THEKERNEL_ABI_ASSERT unix-write-credentials.raw-differential %s pass\n", assertion);
}

static void rights_after_sender_exit(int type, int mode)
{
    int pair[2] = {-1, -1}, listener = -1, gate[2];
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    if (pipe(gate)) fail("rights-gate");
    if (mode == 0) {
        if (socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, pair)) fail("rights-pair");
    } else {
        snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1,
            "thekernel-rights-%d-%d-%d", getpid(), type, mode);
        listener = socket(AF_UNIX, type | SOCK_CLOEXEC, 0);
        if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof(address)) ||
            listen(listener, 1)) fail("rights-listener");
    }
    pid_t child = fork();
    if (child < 0) fail("rights-fork");
    if (!child) {
        close(gate[1]);
        if (mode == 0) {
            close(pair[0]);
        } else {
            close(listener);
            pair[1] = socket(AF_UNIX, type | SOCK_CLOEXEC, 0);
            if (pair[1] < 0 || connect(pair[1], (struct sockaddr *)&address, sizeof(address))) _exit(11);
        }
        char payload = 'r';
        if (mode == 1 && read(gate[0], &payload, 1) != 1) _exit(12);
        int file = memfd_create("queued-rights", MFD_CLOEXEC | MFD_ALLOW_SEALING);
        if (file < 0 || write(file, "queued", 6) != 6 ||
            fcntl(file, F_ADD_SEALS, F_SEAL_WRITE | F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL)) _exit(13);
        union { struct cmsghdr align; unsigned char bytes[CMSG_SPACE(sizeof(int))]; } control = {0};
        struct iovec vec = {.iov_base = &payload, .iov_len = 1};
        struct msghdr msg = {.msg_iov = &vec, .msg_iovlen = 1,
            .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes)};
        struct cmsghdr *header = CMSG_FIRSTHDR(&msg);
        header->cmsg_level = SOL_SOCKET;
        header->cmsg_type = SCM_RIGHTS;
        header->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(header), &file, sizeof(file));
        if (sendmsg(pair[1], &msg, 0) != 1) _exit(14);
        close(file);
        close(pair[1]);
        _exit(0);
    }
    close(gate[0]);
    if (mode == 0) close(pair[1]);
    if (mode == 1) {
        pair[0] = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
        if (pair[0] < 0 || write(gate[1], "r", 1) != 1) fail("rights-accept-before-send");
    }
    close(gate[1]);
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status))
        fail("rights-sender-exit");
    if (mode == 2) {
        pair[0] = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
        if (pair[0] < 0) fail("rights-accept-after-exit");
    }
    if (listener >= 0) close(listener);
    char payload = 0;
    union { struct cmsghdr align; unsigned char bytes[CMSG_SPACE(sizeof(int))]; } control = {0};
    struct iovec vec = {.iov_base = &payload, .iov_len = 1};
    struct msghdr msg = {.msg_iov = &vec, .msg_iovlen = 1,
        .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes)};
    if (recvmsg(pair[0], &msg, MSG_CMSG_CLOEXEC) != 1 || payload != 'r' ||
        (msg.msg_flags & MSG_CTRUNC)) fail("rights-receive-after-exit");
    struct cmsghdr *header = CMSG_FIRSTHDR(&msg);
    if (!header || header->cmsg_level != SOL_SOCKET || header->cmsg_type != SCM_RIGHTS ||
        header->cmsg_len != CMSG_LEN(sizeof(int))) fail("rights-header-after-exit");
    int file;
    memcpy(&file, CMSG_DATA(header), sizeof(file));
    char contents[6];
    int fd_flags = fcntl(file, F_GETFD), seals = fcntl(file, F_GET_SEALS);
    if (pread(file, contents, sizeof(contents), 0) != sizeof(contents) ||
        memcmp(contents, "queued", sizeof(contents)) || fd_flags < 0 || seals < 0 ||
        !(fd_flags & FD_CLOEXEC) || !(seals & F_SEAL_WRITE)) fail("rights-file-after-exit");
    close(file);
    close(pair[0]);
}

static void socket_identity(void)
{
    const int types[] = {SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET};
    const int options[] = {SO_DOMAIN, SO_TYPE, SO_PROTOCOL};
    for (unsigned t = 0; t < sizeof(types) / sizeof(types[0]); t++) {
        int pair[2];
        if (socketpair(AF_UNIX, types[t] | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, pair))
            fail("identity-pair");
        int unbound = socket(AF_UNIX, types[t] | SOCK_CLOEXEC, 0);
        if (unbound < 0) fail("identity-unbound");
        const int fds[] = {pair[0], pair[1], unbound};
        const int expected[] = {AF_UNIX, types[t], 0};
        for (unsigned f = 0; f < 3; f++) {
            for (unsigned o = 0; o < 3; o++) {
                int value = -1;
                socklen_t len = sizeof(value);
                if (getsockopt(fds[f], SOL_SOCKET, options[o], &value, &len) ||
                    value != expected[o] || len != sizeof(value))
                    fail("unix-socket-identity");
            }
            close(fds[f]);
        }
    }
    mark("UNIX_SOCKET_IDENTITY");
}

int main(int argc, char **argv)
{
    alarm(30);
    puts("THEKERNEL_ABI_CASE unix-write-credentials.raw-differential");
    socket_identity();
    int pair[2], one = 1;
    int change_ids = geteuid() == 0;
    if (argc > 1 && strcmp(argv[1], "--require-id-change") == 0 && !change_ids)
        fail("root-required-for-real-effective-test");
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, pair) ||
        setsockopt(pair[0], SOL_SOCKET, SO_PASSCRED, &one, sizeof(one))) fail("socketpair-passcred");
    struct ucred peer;
    socklen_t peer_len = sizeof(peer);
    if (getsockopt(pair[0], SOL_SOCKET, SO_PEERCRED, &peer, &peer_len) ||
        peer_len != sizeof(peer)) fail("peer-credentials");
    if (peer.pid != getpid() || peer.uid != geteuid() || peer.gid != getegid()) {
        fprintf(stderr, "peer expected=%d/%u/%u actual=%d/%u/%u\n",
            getpid(), geteuid(), getegid(), peer.pid, peer.uid, peer.gid);
        fail("peer-identity");
    }
    mark("PEER_PID_EFFECTIVE_IDS");
    uid_t real_uid = getuid();
    gid_t real_gid = getgid();
    pid_t child = fork();
    if (child < 0) fail("fork");
    if (!child) {
        close(pair[0]);
        if (change_ids && (setegid(1) || seteuid(1))) _exit(2);
        if (change_ids && (getuid() != real_uid || getgid() != real_gid ||
            geteuid() != 1 || getegid() != 1 || real_uid == 1 || real_gid == 1)) _exit(6);
        char payload = 'w';
        if (write(pair[1], &payload, 1) != 1) _exit(3);
        payload = 'v';
        struct iovec vec = {.iov_base = &payload, .iov_len = 1};
        if (writev(pair[1], &vec, 1) != 1) _exit(4);
        payload = 'm';
        struct msghdr msg = {.msg_iov = &vec, .msg_iovlen = 1};
        if (sendmsg(pair[1], &msg, 0) != 1) _exit(5);
        _exit(0);
    }
    close(pair[1]);
    for (unsigned i = 0; i < 3; i++) {
        char payload = 0;
        union { struct cmsghdr align; unsigned char bytes[CMSG_SPACE(sizeof(struct ucred))]; } control;
        memset(&control, 0, sizeof(control));
        struct iovec vec = {.iov_base = &payload, .iov_len = 1};
        struct msghdr msg = {.msg_iov = &vec, .msg_iovlen = 1,
            .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes)};
        struct pollfd ready = {.fd = pair[0], .events = POLLIN};
        if (poll(&ready, 1, 5000) != 1) fail("worker-message-timeout");
        if (recvmsg(pair[0], &msg, 0) != 1 || payload != "wvm"[i] || (msg.msg_flags & MSG_CTRUNC)) fail("receive");
        struct cmsghdr *header = CMSG_FIRSTHDR(&msg);
        if (!header || header->cmsg_level != SOL_SOCKET || header->cmsg_type != SCM_CREDENTIALS ||
            header->cmsg_len != CMSG_LEN(sizeof(struct ucred))) fail("credentials-header");
        struct ucred creds;
        memcpy(&creds, CMSG_DATA(header), sizeof(creds));
        if (creds.pid != child || creds.uid != real_uid || creds.gid != real_gid) {
            fprintf(stderr, "sender message=%u expected=%d/%u/%u actual=%d/%u/%u\n",
                i, child, real_uid, real_gid, creds.pid, creds.uid, creds.gid);
            fail("sender-identity");
        }
        static const char *assertions[] = {
            "WRITE_SENDER_PID_REAL_IDS", "WRITEV_SENDER_PID_REAL_IDS", "SENDMSG_SENDER_PID_REAL_IDS"
        };
        mark(assertions[i]);
    }
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) fail("child-exit");
    mark("CHILD_EXIT_CLEAN");
    if (change_ids) mark("REAL_EFFECTIVE_IDS");
    close(pair[0]);
    for (int mode = 0; mode < 3; mode++) {
        rights_after_sender_exit(SOCK_STREAM, mode);
        rights_after_sender_exit(SOCK_SEQPACKET, mode);
    }
    rights_after_sender_exit(SOCK_DGRAM, 0);
    mark("RIGHTS_RECEIVER_LIFETIME");
    puts("THEKERNEL_ABI_RESULT unix-write-credentials.raw-differential pass");
    puts("THEKERNEL_UNIX_WRITE_CREDENTIALS_OK");
    if (change_ids) puts("THEKERNEL_UNIX_WRITE_CREDENTIALS_REAL_EFFECTIVE_OK");
    return 0;
}
