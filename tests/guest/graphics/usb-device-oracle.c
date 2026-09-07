#define _GNU_SOURCE
/* QEMU USB acceptance: explicit synthetic disk and BUS_USB evdev devices. */
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

static int fail(const char *stage) {
    printf("TK_USB stage=%s state=FAIL errno=%d\n", stage, errno);
    return 1;
}
static int storage(const char *path) {
    unsigned char signature[32] = {0}, data[512], verify[512];
    int fd = open(path, O_RDWR | O_CLOEXEC);
    if (fd < 0) return fail("block-open");
    if (pread(fd, signature, sizeof(signature), 0) != sizeof(signature) ||
        memcmp(signature, "THEKERNEL_USB_TEST_DISK", 23)) {
        close(fd); errno = EINVAL; return fail("synthetic-disk-signature");
    }
    for (size_t i = 0; i < sizeof(data); i++) data[i] = (unsigned char)(i * 37 + 19);
    if (pwrite(fd, data, sizeof(data), 4096) != sizeof(data) || fsync(fd)) {
        close(fd); return fail("block-write-flush");
    }
    close(fd);
    fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0 || pread(fd, verify, sizeof(verify), 4096) != sizeof(verify) ||
        memcmp(data, verify, sizeof(data))) {
        if (fd >= 0) close(fd);
        return fail("block-readback");
    }
    close(fd);
    printf("TK_USB stage=block-read-write-flush state=OK device=%s bytes=512 offset=4096\n", path);
    return 0;
}
static long seconds(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec; }
int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc != 2) { errno = EINVAL; return fail("usage-usb-device-oracle-synthetic-block-path"); }
    if (storage(argv[1])) return 1;
    struct pollfd devices[2] = {{.fd = -1, .events = POLLIN}, {.fd = -1, .events = POLLIN}};
    for (int index = 0; index < 32; index++) {
        char path[64], name[128] = {0}; struct input_id id;
        snprintf(path, sizeof(path), "/dev/input/event%d", index);
        int fd = open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
        if (fd < 0) continue;
        if (ioctl(fd, EVIOCGID, &id) || id.bustype != BUS_USB || ioctl(fd, EVIOCGNAME(sizeof(name)), name) < 0) { close(fd); continue; }
        int slot = strstr(name, "keyboard") ? 0 : strstr(name, "mouse") ? 1 : -1;
        if (slot < 0 || devices[slot].fd >= 0) { close(fd); continue; }
        devices[slot].fd = fd;
        printf("TK_USB stage=input-enumeration state=OK path=%s name=%s bus=%u\n", path, name, id.bustype);
    }
    if (devices[0].fd < 0 || devices[1].fd < 0) { errno = ENODEV; return fail("keyboard-mouse-enumeration"); }
    puts("THEKERNEL_USB_INPUT_READY");
    int key_down = 0, key_up = 0, button_down = 0, button_up = 0, x = 0, y = 0, release_ready = 0;
    long deadline = seconds() + 30;
    while (seconds() < deadline) {
        int count = poll(devices, 2, 100);
        if (count < 0 && errno != EINTR) return fail("input-poll");
        for (int device = 0; device < 2; device++) {
            if (!(devices[device].revents & POLLIN)) continue;
            struct input_event events[32];
            ssize_t length = read(devices[device].fd, events, sizeof(events));
            if (length < 0 && errno == EAGAIN) continue;
            if (length < 0 || length % sizeof(events[0])) return fail("input-read");
            for (size_t index = 0; index < (size_t)length / sizeof(events[0]); index++) {
                struct input_event e = events[index];
                if (device == 0 && e.type == EV_KEY && e.code == KEY_A) {
                    if (e.value == 1) key_down = 1;
                    if (e.value == 0 && key_down) key_up = 1;
                }
                if (device == 1 && e.type == EV_KEY && e.code == BTN_LEFT) {
                    if (e.value == 1) button_down = 1;
                    if (e.value == 0 && button_down) button_up = 1;
                }
                if (device == 1 && e.type == EV_REL && e.code == REL_X) x += e.value;
                if (device == 1 && e.type == EV_REL && e.code == REL_Y) y += e.value;
            }
        }
        if (!release_ready && key_down && button_down && x == 12 && y == -7) {
            release_ready = 1; puts("THEKERNEL_USB_RELEASE_READY");
        }
        if (release_ready && key_up && button_up) {
            close(devices[0].fd); close(devices[1].fd);
            puts("TK_USB stage=keyboard-mouse-events state=OK key=30 button=272 dx=12 dy=-7");
            puts("THEKERNEL_USB_ACCEPTANCE_OK");
            return 0;
        }
    }
    printf("TK_USB stage=input-timeout state=FAIL key=%d/%d button=%d/%d xy=%d/%d\n", key_down, key_up, button_down, button_up, x, y);
    return 1;
}
