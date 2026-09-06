#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
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
int main(int argc, char **argv)
{
    puts("THEKERNEL_ABI_CASE unix-write-credentials.raw-differential");
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
    puts("THEKERNEL_ABI_RESULT unix-write-credentials.raw-differential pass");
    puts("THEKERNEL_UNIX_WRITE_CREDENTIALS_OK");
    if (change_ids) puts("THEKERNEL_UNIX_WRITE_CREDENTIALS_REAL_EFFECTIVE_OK");
    return 0;
}
