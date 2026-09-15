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

static int expect_data(int fd, const void *expected, size_t length, const char *label) {
    unsigned char data[64];
    if (length > sizeof(data)) return -1;
    size_t offset = 0;
    while (offset < length) {
        struct pollfd p = {.fd = fd, .events = POLLIN};
        if (poll(&p, 1, 5000) != 1) {
            fprintf(stderr, "%s: data timeout errno=%d\n", label, errno);
            return -1;
        }
        ssize_t n = read(fd, data + offset, length - offset);
        if (n <= 0) {
            fprintf(stderr, "%s: read=%zd errno=%d\n", label, n, errno);
            return -1;
        }
        offset += (size_t)n;
    }
    if (memcmp(data, expected, length)) {
        fprintf(stderr, "%s: data mismatch length=%zu\n", label, length);
        return -1;
    }
    return 0;
}

/* Kernel termios2, distinct from libc's larger struct termios. */
struct linux_termios2 {
    unsigned int iflag, oflag, cflag, lflag;
    unsigned char line, cc[19];
    unsigned int ispeed, ospeed;
};

static int line_semantics(void) {
    int master = posix_openpt(O_RDWR | O_NOCTTY | O_NONBLOCK);
    int slave = -1;
    const char *step = "open PTY";
    if (master < 0 || grantpt(master) || unlockpt(master)) goto fail;
    char *path = ptsname(master);
    slave = path ? open(path, O_RDWR | O_NOCTTY | O_NONBLOCK) : -1;
    if (slave < 0) goto fail;
    struct termios term, got;
    step = "common stty flags and baud";
    if (tcgetattr(slave, &term)) goto fail;
    term.c_iflag |= IXON;
    term.c_lflag |= ICANON | ECHO | ECHOE | ECHOK | ECHOKE | IEXTEN | TOSTOP;
    term.c_lflag &= ~ECHONL;
    term.c_cc[VMIN] = 1;
    term.c_cc[VTIME] = 0;
    term.c_cc[VERASE] = 127;
    term.c_cc[VWERASE] = 23;
    if (cfsetispeed(&term, B115200) || cfsetospeed(&term, B115200) ||
        tcsetattr(slave, TCSANOW, &term) || tcgetattr(slave, &got) ||
        !(got.c_iflag & IXON) || !(got.c_lflag & TOSTOP) ||
        cfgetospeed(&got) != B115200 || got.c_cc[VWERASE] != 23) goto fail;
    step = "TCSETS2 BOTHER";
    struct linux_termios2 raw;
    if (ioctl(slave, _IOR('T', 0x2a, struct linux_termios2), &raw)) goto fail;
    raw.cflag = (raw.cflag & ~(0x100fu | 0x100f0000u)) | 0x1000u | 0x10000000u;
    raw.ispeed = 12345;
    raw.ospeed = 56789;
    if (ioctl(slave, _IOW('T', 0x2b, struct linux_termios2), &raw) ||
        ioctl(slave, _IOR('T', 0x2a, struct linux_termios2), &raw) ||
        raw.ispeed != 12345 || raw.ospeed != 56789 || tcsetattr(slave, TCSANOW, &term)) goto fail;
    step = "common stty output modes";
    struct termios mapped = term;
    mapped.c_oflag = OPOST | ONLCR | ONLRET | ONOCR | OCRNL | OLCUC | TAB3;
    if (tcsetattr(slave, TCSANOW, &mapped) || write(slave, "\ra\tb\n", 5) != 5 ||
        expect_data(master, "A       B\r\n", 11, step) || tcsetattr(slave, TCSANOW, &term)) goto fail;
    step = "empty erase echo";
    if (write(master, "\177", 1) != 1) goto fail;
    struct pollfd p = {.fd = master, .events = POLLIN};
    if (poll(&p, 1, 100) != 0) goto fail;
    step = "Tab erase preserves prompt columns";
    if (write(slave, "> ", 2) != 2 || expect_data(master, "> ", 2, step) ||
        write(master, "\t\177\n", 3) != 3 ||
        expect_data(master, "\t\b\b\b\b\b\b\r\n", 9, step) ||
        expect_data(slave, "\n", 1, step)) goto fail;
    step = "canonical Tab and non-graphic input";
    const unsigned char input[] = {'a', '\t', 0, 1, 0x80, 0xff, '\n'};
    if (write(master, input, sizeof(input)) != sizeof(input) ||
        expect_data(slave, input, sizeof(input), step) ||
        expect_data(master, "a\t^@^A\200\377\r\n", 10, "canonical echo")) goto fail;
    if (tcflush(slave, TCIOFLUSH)) goto fail;
    step = "VMIN poll and nonblocking read";
    term.c_lflag &= ~(ICANON | ECHO | ECHONL);
    term.c_cc[VMIN] = 3;
    if (tcsetattr(slave, TCSANOW, &term) || write(master, "ab", 2) != 2) goto fail;
    p = (struct pollfd){.fd = slave, .events = POLLIN};
    if (poll(&p, 1, 100) != 0) goto fail;
    unsigned char data[8];
    /* O_NONBLOCK read ignores VMIN even though poll obeys it. */
    if (read(slave, data, sizeof(data)) != 2 || memcmp(data, "ab", 2)) goto fail;
    if (write(master, "abc", 3) != 3 || expect_data(slave, "abc", 3, step)) goto fail;
    term.c_cc[VMIN] = 255;
    term.c_cc[VTIME] = 1;
    if (tcsetattr(slave, TCSANOW, &term) || write(master, "z", 1) != 1 ||
        expect_data(slave, "z", 1, "VTIME poll")) goto fail;
    step = "IXON STOP/START and disable";
    term.c_cc[VMIN] = 1;
    term.c_cc[VTIME] = 0;
    if (tcsetattr(slave, TCSANOW, &term) || write(master, "\023x", 2) != 2 ||
        expect_data(slave, "x", 1, step)) goto fail;
    errno = 0;
    if (write(slave, "o", 1) != -1 || errno != EAGAIN) goto fail;
    if (write(master, "\021x", 2) != 2 || expect_data(slave, "x", 1, step) ||
        write(slave, "out", 3) != 3 || expect_data(master, "out", 3, step)) goto fail;
    if (write(master, "\023x", 2) != 2 || expect_data(slave, "x", 1, step)) goto fail;
    step = "explicit output stop survives IXON resume and disable";
    if (tcflow(slave, TCOOFF) || write(master, "\021x", 2) != 2 ||
        expect_data(slave, "x", 1, step)) goto fail;
    errno = 0;
    if (write(slave, "o", 1) != -1 || errno != EAGAIN) goto fail;
    term.c_iflag &= ~IXON;
    if (tcsetattr(slave, TCSANOW, &term)) goto fail;
    errno = 0;
    if (write(slave, "o", 1) != -1 || errno != EAGAIN || tcflow(slave, TCOON)) goto fail;
    if (write(slave, "ok", 2) != 2 ||
        expect_data(master, "ok", 2, step)) goto fail;
    close(slave);
    close(master);
    puts("THEKERNEL_TTY_LINE_SEMANTICS_OK");
    return 0;
fail:
    fprintf(stderr, "tty line semantics: %s errno=%d (%s)\n", step, errno, strerror(errno));
    if (slave >= 0) close(slave);
    if (master >= 0) close(master);
    return 1;
}

int main(void) {
    if (line_semantics()) return 1;
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
