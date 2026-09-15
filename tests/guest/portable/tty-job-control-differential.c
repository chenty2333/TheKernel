#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

#ifndef TIOCGPTPEER
#define TIOCGPTPEER 0x5441
#endif

static int check(int ok, const char *name) {
    if (!ok) fprintf(stderr, "TTY_JOB_FAIL %s errno=%d\n", name, errno);
    return ok ? 0 : -1;
}

static int stopped_child(int slave, int change_group) {
    pid_t child = fork();
    if (child < 0) return -1;
    if (!child) {
        alarm(5);
        signal(SIGTTIN, SIG_DFL);
        signal(SIGTTOU, SIG_DFL);
        if (setpgid(0, 0)) _exit(70);
        if (change_group) {
            if (tcsetpgrp(slave, getpgrp()) < 0) _exit(71);
        } else {
            char byte;
            (void)read(slave, &byte, 1);
        }
        _exit(72); /* Both operations must stop, never silently return. */
    }
    int status = 0;
    pid_t waited;
    do { waited = waitpid(child, &status, WUNTRACED); } while (waited < 0 && errno == EINTR);
    int ok = waited == child && WIFSTOPPED(status) &&
             WSTOPSIG(status) == (change_group ? SIGTTOU : SIGTTIN);
    kill(child, SIGKILL);
    kill(child, SIGCONT);
    while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
    return check(ok, change_group ? "background-tiocspgrp-SIGTTOU" : "background-read-SIGTTIN");
}

static int job_control(void) {
    alarm(15);
    signal(SIGHUP, SIG_IGN);
    signal(SIGTTOU, SIG_IGN);
    if (setsid() < 0) return 1;
    int master = posix_openpt(O_RDWR | O_NOCTTY);
    if (master < 0 || grantpt(master) || unlockpt(master)) return 2;
    char *name = ptsname(master);
    int slave = name ? open(name, O_RDWR | O_NOCTTY) : -1;
    if (slave < 0) return 3;
    errno = 0;
    if (check(tcgetpgrp(slave) == -1 && errno == ENOTTY, "O_NOCTTY")) return 4;
    close(slave);
    slave = open(name, O_RDWR);
    if (check(slave >= 0 && tcgetpgrp(slave) == getpgrp(), "automatic-controlling-terminal")) return 5;
    if (stopped_child(slave, 0) || stopped_child(slave, 1)) return 6;
    close(slave);
    close(master);
    return 0;
}

static int packet_byte(int fd, unsigned char expected) {
    struct pollfd pollfd = { .fd = fd, .events = POLLIN | POLLPRI };
    unsigned char data[8];
    if (check(poll(&pollfd, 1, 3000) == 1 && (pollfd.revents & POLLPRI), "packet-POLLPRI")) return -1;
    return check(read(fd, data, sizeof(data)) == 1 && data[0] == expected, "packet-control-byte");
}

static int peer_and_packet(void) {
    int master = posix_openpt(O_RDWR | O_NOCTTY | O_NONBLOCK);
    if (master < 0) return -1;
    errno = 0;
    int slave = ioctl(master, TIOCGPTPEER, O_RDWR | O_NOCTTY);
    if (check(slave == -1 && errno == EIO, "locked-peer")) return -1;
    if (grantpt(master) || unlockpt(master)) return -1;
    slave = ioctl(master, TIOCGPTPEER, O_RDWR | O_NOCTTY | O_NONBLOCK | O_CLOEXEC);
    if (check(slave >= 0 && (fcntl(slave, F_GETFD) & FD_CLOEXEC) &&
              (fcntl(slave, F_GETFL) & O_NONBLOCK), "peer-open-flags")) return -1;
    struct termios term;
    if (tcgetattr(slave, &term)) return -1;
    term.c_lflag &= ~ECHO;
    if (tcsetattr(slave, TCSANOW, &term)) return -1;
    int enabled = 1;
    if (ioctl(master, TIOCPKT, &enabled)) return -1;
    if (tcflow(slave, TCOOFF) || packet_byte(master, TIOCPKT_STOP) ||
        tcflow(slave, TCOON) || packet_byte(master, TIOCPKT_START)) return -1;
    if (write(slave, "data", 4) != 4) return -1;
    struct pollfd p = { .fd = master, .events = POLLIN };
    unsigned char data[32];
    if (check(poll(&p, 1, 3000) == 1 && read(master, data, sizeof(data)) == 5 &&
              !memcmp(data, "\0data", 5), "packet-data-prefix")) return -1;
    if (write(master, "unread\n", 7) != 7) return -1;
    close(master);
    if (check(read(slave, data, sizeof(data)) == 0, "master-close-discards-input")) return -1;
    close(slave);
    return 0;
}

int main(void) {
    alarm(25);
    pid_t child = fork();
    if (child < 0) return 1;
    if (!child) _exit(job_control());
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) {
        fprintf(stderr, "TTY_JOB_FAIL controller status=%d\n", status);
        return 1;
    }
    if (peer_and_packet()) return 1;
    puts("THEKERNEL_ABI_CASE tty-job-control.portable-differential");
    puts("THEKERNEL_ABI_ASSERT tty-job-control.portable-differential AUTO_CTTY_NOCTTY pass");
    puts("THEKERNEL_ABI_ASSERT tty-job-control.portable-differential SIGTTIN_SIGTTOU pass");
    puts("THEKERNEL_ABI_ASSERT tty-job-control.portable-differential PTPEER_PACKET_HANGUP pass");
    puts("THEKERNEL_TTY_JOB_CONTROL_OK");
    puts("THEKERNEL_ABI_RESULT tty-job-control.portable-differential pass");
    return 0;
}
