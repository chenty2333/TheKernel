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

static int signal_fd;
static volatile sig_atomic_t signals;
static volatile sig_atomic_t resizes;
static void interrupted(int signo) {
    int saved = errno;
    if (signo == SIGWINCH)
        resizes++;
    else
        signals++;
    if (write(signal_fd, signo == SIGWINCH ? "W" : "I", 1) != 1)
        _exit(91);
    errno = saved;
}
static int byte(int fd, char expected) {
    struct pollfd p = { .fd = fd, .events = POLLIN };
    int ready;
    do { ready = poll(&p, 1, 5000); } while (ready < 0 && errno == EINTR);
    char c = 0;
    ssize_t n;
    do { n = ready > 0 ? read(fd, &c, 1) : -1; } while (n < 0 && errno == EINTR);
    return n == 1 && c == expected ? 0 : -1;
}
static int child(int slave, int command, int report) {
    signal_fd = report;
    if (setsid() < 0 || ioctl(slave, TIOCSCTTY, 0) < 0)
        return 10;
    struct sigaction sa = { .sa_handler = interrupted };
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGINT, &sa, NULL) < 0 || sigaction(SIGWINCH, &sa, NULL) < 0)
        return 11;
    struct termios saved, raw;
    if (tcgetattr(slave, &saved) < 0)
        return 12;
    raw = saved;
    /* CPython 3.14 _pyrepl.unix_console.UnixConsole.prepare, not cfmakeraw:
       signal delivery deliberately stays enabled in this cbreak profile. */
    raw.c_iflag &= ~(INPCK | ISTRIP | IXON);
    raw.c_iflag |= BRKINT;
    raw.c_oflag &= ~OPOST;
    raw.c_cflag &= ~(CSIZE | PARENB);
    raw.c_cflag |= CS8;
    raw.c_lflag &= ~(ICANON | ECHO | IEXTEN);
    raw.c_lflag |= ISIG;
    raw.c_cc[VMIN] = 1;
    raw.c_cc[VTIME] = 0;
    if (tcsetattr(slave, TCSADRAIN, &raw) < 0)
        return 13;
    struct termios got;
    if (tcgetattr(slave, &got) < 0 || (got.c_lflag & (ICANON | ECHO | ISIG)) != ISIG ||
        !(got.c_iflag & BRKINT) || got.c_cc[VMIN] != 1 || got.c_cc[VTIME] != 0)
        return 14;
    for (int phase = 0; phase < 3; phase++) {
        if (phase) {
            if (phase == 1)
                raw.c_lflag |= NOFLSH;
            else {
                raw.c_lflag &= ~ISIG;
                raw.c_iflag &= ~ICRNL;
            }
            if (tcsetattr(slave, TCSANOW, &raw) < 0)
                return 15;
        }
        if (write(report, "R", 1) != 1 || byte(command, 'G') < 0)
            return 16;
        const char *expected = phase == 0 ? "after" : phase == 1 ? "keeptail" : "\033:wq\r";
        char data[32];
        size_t total = 0;
        while (total < strlen(expected)) {
            ssize_t n = read(slave, data + total, strlen(expected) - total);
            if (n <= 0)
                return 17;
            total += (size_t)n;
        }
        if (memcmp(data, expected, total) || signals != (phase == 0 ? 1 : 2))
            return 17;
        if (write(report, "D", 1) != 1)
            return 18;
    }
    if (write(report, "R", 1) != 1 || byte(command, 'G') < 0)
        return 19;
    struct winsize size;
    if (ioctl(slave, TIOCGWINSZ, &size) < 0 || size.ws_row != 31 || size.ws_col != 97 ||
        size.ws_xpixel != 777 || size.ws_ypixel != 444 || resizes != 1)
        return 20;
    if (write(report, "D", 1) != 1 || tcsetattr(slave, TCSADRAIN, &saved) < 0)
        return 21;
    return 0;
}
int main(void) {
    int master = posix_openpt(O_RDWR | O_NOCTTY);
    int commands[2], reports[2];
    if (master < 0 || grantpt(master) || unlockpt(master) ||
        pipe(commands) || pipe(reports)) {
        perror("tty-termios setup");
        return 1;
    }
    char *path = ptsname(master);
    int slave = path ? open(path, O_RDWR | O_NOCTTY) : -1;
    if (slave < 0)
        return 1;
    struct winsize initial = { .ws_row = 24, .ws_col = 80 };
    // Before there is a foreground group, resize still succeeds.
    if (ioctl(master, TIOCSWINSZ, &initial) < 0)
        return 1;
    pid_t pid = fork();
    if (pid < 0)
        return 1;
    if (pid == 0) {
        close(master); close(commands[1]); close(reports[0]);
        _exit(child(slave, commands[0], reports[1]));
    }
    close(slave); close(commands[0]); close(reports[1]);
    int failed = 0;
    for (int phase = 0; phase < 3; phase++) {
        const char *input = phase == 0 ? "before\003after" : phase == 1 ? "keep\003tail" : "\033:wq\r";
        if (byte(reports[0], 'R') ||
            write(master, input, strlen(input)) != (ssize_t)strlen(input) ||
            (phase < 2 && byte(reports[0], 'I')) || write(commands[1], "G", 1) != 1 ||
            byte(reports[0], 'D')) {
            failed = 1;
            kill(pid, SIGKILL);
            break;
        }
    }
    if (!failed) {
        struct winsize next = { .ws_row = 31, .ws_col = 97, .ws_xpixel = 777, .ws_ypixel = 444 };
        if (byte(reports[0], 'R') || ioctl(master, TIOCSWINSZ, &next) < 0 ||
            byte(reports[0], 'W') || ioctl(master, TIOCSWINSZ, &next) < 0 ||
            write(commands[1], "G", 1) != 1 || byte(reports[0], 'D')) {
            failed = 1;
            kill(pid, SIGKILL);
        }
    }
    int status = -1;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || WEXITSTATUS(status))
        failed = 1;
    close(master); close(commands[1]); close(reports[0]);
    if (failed) {
        fprintf(stderr, "THEKERNEL_TTY_TERMIOS_FAIL status=%d errno=%d\n", status, errno);
        return 1;
    }
    puts("THEKERNEL_ABI_CASE tty-termios.portable-differential");
    puts("THEKERNEL_ABI_ASSERT tty-termios.portable-differential PYREPL_PREPARE_RESTORE pass");
    puts("THEKERNEL_ABI_ASSERT tty-termios.portable-differential CBREAK_SIGINT_FLUSH pass");
    puts("THEKERNEL_ABI_ASSERT tty-termios.portable-differential NOFLSH_PRESERVES_INPUT pass");
    puts("THEKERNEL_ABI_ASSERT tty-termios.portable-differential RAW_ESCAPE_CR_BYTES pass");
    puts("THEKERNEL_ABI_ASSERT tty-termios.portable-differential WINSIZE_SIGWINCH_NOOP pass");
    puts("THEKERNEL_TTY_TERMIOS_OK");
    puts("THEKERNEL_ABI_RESULT tty-termios.portable-differential pass");
    return 0;
}
