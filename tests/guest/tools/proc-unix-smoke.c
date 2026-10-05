#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static void need(int ok, const char *what) {
    if (!ok) { fprintf(stderr, "proc-unix: %s errno=%d (%s)\n", what, errno, strerror(errno)); exit(1); }
}
struct row { unsigned refs, protocol, flags, type, state; char path[256]; };
static ino_t inode_of(int fd) { struct stat st; need(!fstat(fd, &st), "socket fstat"); return st.st_ino; }
static int rows(FILE *file, ino_t inode, struct row *result) {
    char line[1024];
    need(fgets(line, sizeof(line), file) && !strcmp(line, "Num       RefCount Protocol Flags    Type St Inode Path\n"), "Unix header");
    int found = 0;
    while (fgets(line, sizeof(line), file)) {
        unsigned long long number; char opaque[65]; struct row row = {0}; int offset = 0;
        need(sscanf(line, "%64[0-9a-fA-F]: %x %x %x %x %x %llu%n", opaque, &row.refs, &row.protocol,
            &row.flags, &row.type, &row.state, &number, &offset) == 7, "Unix prefix grammar");
        need(strlen(opaque) == 16 && !row.protocol && line[strlen(line)-1] == '\n', "pointer width, protocol, newline");
        if (number != (unsigned long long)inode) continue;
        const char *path = line + offset; if (*path == ' ') path++;
        size_t size = strlen(path); if (size && path[size-1] == '\n') size--;
        need(size < sizeof(row.path), "test pathname capacity"); memcpy(row.path, path, size);
        *result = row; found++;
    }
    need(!ferror(file), "Unix table read"); return found;
}
static int lookup(ino_t inode, struct row *row) {
    FILE *file = fopen("/proc/net/unix", "r"); need(file != NULL, "Unix table open");
    int found = rows(file, inode, row); fclose(file); return found;
}
static void check(int fd, unsigned type, unsigned state, unsigned flags, const char *path) {
    struct row row; need(lookup(inode_of(fd), &row) == 1, "one live socket inode");
    if (row.type != type || row.state != state || row.flags != flags || strcmp(row.path, path))
        fprintf(stderr, "proc-unix fd=%d inode=%llu actual=%x/%x/%x path=%s expected=%x/%x/%x path=%s\n",
            fd, (unsigned long long)inode_of(fd), row.type, row.state, row.flags, row.path, type, state, flags, path);
    need(row.type == type && row.state == state && row.flags == flags && !strcmp(row.path, path), "live type, state, flags, address");
    need(row.refs > 0, "real owner reference count");
}
static void gone(ino_t inode) { struct row row; need(!lookup(inode, &row), "final OFD close retires inode"); }
static void pair(unsigned type) {
    int sockets[2]; need(!socketpair(AF_UNIX, type | SOCK_CLOEXEC, 0, sockets), "socketpair");
    check(sockets[0], type, 3, 0, ""); check(sockets[1], type, 3, 0, "");
    need(send(sockets[0], "q", 1, 0) == 1, "pair enqueue"); check(sockets[1], type, 3, 0, "");
    char byte; need(recv(sockets[1], &byte, 1, 0) == 1 && byte == 'q', "table did not consume pair data");
    ino_t inode = inode_of(sockets[0]); int duplicate = dup(sockets[0]); need(duplicate >= 0, "pair duplicate");
    close(sockets[0]); check(duplicate, type, 3, 0, ""); close(duplicate); gone(inode); close(sockets[1]);
}
static void tools(const char *path, int busybox) {
    int output[2]; need(!pipe(output), "tool output pipe"); pid_t child = fork(); need(child >= 0, "tool fork");
    if (!child) {
        close(output[0]); dup2(output[1], STDOUT_FILENO); dup2(output[1], STDERR_FILENO); close(output[1]);
        if (busybox) execl("/opt/thekernel-tools/bin/busybox", "busybox", "netstat", "-xanp", (char *)NULL);
        else execl("/opt/thekernel-tools/bin/netstat", "netstat", "-xanp", (char *)NULL);
        _exit(127);
    }
    close(output[1]); char text[32768]; size_t used = 0; ssize_t amount;
    while (used < sizeof(text)-1 && (amount = read(output[0], text+used, sizeof(text)-1-used)) > 0) used += amount;
    need(used < sizeof(text)-1, "tool output bounded"); text[used] = 0; close(output[0]); int status;
    need(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0, "real netstat exit");
    printf("UNIX_NETSTAT_PROVIDER=%s\n%s", busybox ? "busybox" : "net-tools", text);
    char *line = strstr(text, path); need(line != NULL, "real netstat bound pathname");
    char *start = line; while (start > text && start[-1] != '\n') start--;
    need(strstr(start, "LISTENING") && strstr(start, "STREAM"), "real netstat listener type and state");
    if (busybox) {
        char owner[32]; snprintf(owner, sizeof(owner), "%ld/", (long)getpid());
        char *end = strchr(start, '\n'), *pid = strstr(start, owner);
        need(pid && (!end || pid < end), "BusyBox real listener PID owner");
    }
}
static void namespace_view(int old_socket) {
    ino_t old_inode = inode_of(old_socket); int saved = open("/proc/self/ns/net", O_RDONLY|O_CLOEXEC);
    need(saved >= 0, "saved network namespace"); FILE *old = fopen("/proc/net/unix", "r"); need(old != NULL, "pinned old table");
    need(!unshare(CLONE_NEWNET), "new network namespace"); struct row row;
    need(rows(old, old_inode, &row) == 1, "opened table pins original namespace");
    need(!lookup(old_inode, &row), "fresh table excludes inherited socket in old namespace");
    int fresh = socket(AF_UNIX, SOCK_DGRAM|SOCK_CLOEXEC, 0); need(fresh >= 0, "new namespace socket");
    check(fresh, 2, 1, 0, ""); ino_t fresh_inode = inode_of(fresh);
    need(!setns(saved, CLONE_NEWNET), "restore network namespace");
    need(lookup(old_inode, &row) == 1 && !lookup(fresh_inode, &row), "restored table isolates new namespace socket");
    fclose(old); close(saved); close(fresh);
}
int main(int argc, char **argv) {
    int run_tools = 0, run_namespace = 0;
    for (int arg = 1; arg < argc; arg++) { if (!strcmp(argv[arg], "--tools")) run_tools = 1;
        else if (!strcmp(argv[arg], "--namespace")) run_namespace = 1; else need(0, "unknown argument"); }
    pair(SOCK_STREAM); pair(SOCK_DGRAM); pair(SOCK_SEQPACKET);
    for (unsigned type = SOCK_STREAM; type <= SOCK_SEQPACKET; type++) {
        if (type != SOCK_STREAM && type != SOCK_DGRAM && type != SOCK_SEQPACKET) continue;
        int fd = socket(AF_UNIX, type|SOCK_CLOEXEC, 0); need(fd >= 0, "unbound Unix socket"); check(fd, type, 1, 0, "");
        struct sockaddr_un address = {.sun_family = AF_UNIX};
        int size = snprintf(address.sun_path+1, sizeof(address.sun_path)-1, "tk-unix-%ld-%u", (long)getpid(), type);
        address.sun_path[size+1] = 0; address.sun_path[size+2] = 'b'; address.sun_path[size+3] = '\xff';
        need(!bind(fd, (void *)&address, offsetof(struct sockaddr_un, sun_path)+size+4), "raw abstract bind");
        char path[128]; snprintf(path, sizeof(path), "@tk-unix-%ld-%u@b\xff", (long)getpid(), type);
        check(fd, type, 1, 0, path);
        if (type != SOCK_DGRAM) { need(!listen(fd, 2), "abstract listen"); check(fd, type, 1, 0x10000, path); }
        ino_t inode = inode_of(fd); close(fd); gone(inode);
    }
    char path[108]; snprintf(path, sizeof(path), "/tmp/tk-unix-%ld.sock", (long)getpid()); unlink(path);
    int listener = socket(AF_UNIX, SOCK_STREAM|SOCK_CLOEXEC, 0); need(listener >= 0, "path listener");
    struct sockaddr_un address = {.sun_family = AF_UNIX}; strcpy(address.sun_path, path);
    need(!bind(listener, (void *)&address, offsetof(struct sockaddr_un, sun_path)+strlen(path)+1), "pathname bind");
    check(listener, 1, 1, 0, path); need(!listen(listener, 4), "pathname listen"); check(listener, 1, 1, 0x10000, path);
    int client = socket(AF_UNIX, SOCK_STREAM|SOCK_CLOEXEC, 0); need(client >= 0, "client socket");
    need(!connect(client, (void *)&address, offsetof(struct sockaddr_un, sun_path)+strlen(path)+1), "pathname connect");
    int server = accept4(listener, NULL, NULL, SOCK_CLOEXEC); need(server >= 0, "accept new observed OFD");
    check(server, 1, 3, 0, path); check(client, 1, 3, 0, "");
    need(send(client, "not-consumed", 13, 0) == 13, "stream enqueue"); check(server, 1, 3, 0, path);
    if (run_tools) { tools(path, 0); tools(path, 1); }
    if (run_namespace) namespace_view(listener);
    char data[13]; need(recv(server, data, sizeof(data), MSG_WAITALL) == sizeof(data) && !memcmp(data, "not-consumed", 13), "diagnostics preserve stream data");
    ino_t server_inode = inode_of(server), client_inode = inode_of(client), listener_inode = inode_of(listener);
    close(server); close(client); close(listener); gone(server_inode); gone(client_inode); gone(listener_inode); unlink(path);
    puts("PROC_UNIX_OK real types/states/paths/inodes, duplicate/final-close and non-consuming observations"); return 0;
}
