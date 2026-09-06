#define _GNU_SOURCE
/* Build: cc -std=c11 -Wall -Wextra -Werror -o evdev-uapi-oracle evdev-uapi-oracle.c */
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "graphics oracle requires the x86_64 Linux ABI"
#endif

_Static_assert(sizeof(struct input_event) == 24, "input_event x86_64 ABI");
_Static_assert(sizeof(struct input_id) == 8, "input_id ABI");
_Static_assert(sizeof(struct input_absinfo) == 24, "input_absinfo ABI");

static int failures;

static void result(const char *kind, const char *state, int error) {
    if (strcmp(state, "FAIL") == 0) failures++;
    printf("TK_GRAPHICS kind=%s state=%s errno=%d\n", kind, state, error);
}

static int open_event(void) {
    char path[32];
    for (int i = 0; i < 32; ++i) {
        snprintf(path, sizeof(path), "/dev/input/event%d", i);
        int fd = open(path, O_RDWR | O_CLOEXEC | O_NONBLOCK);
        if (fd >= 0) return fd;
    }
    return -1;
}

static void bitset_hex(const unsigned char *bits, size_t count, char *out, size_t out_size) {
    static const char hex[] = "0123456789abcdef";
    size_t used = 0;
    for (size_t i = 0; i < count && used + 2 < out_size; ++i) {
        out[used++] = hex[bits[i] >> 4]; out[used++] = hex[bits[i] & 15];
    }
    out[used] = '\0';
}

static size_t bitmap_bytes(unsigned max_bit) {
    const size_t word_bits = sizeof(unsigned long) * 8;
    return ((max_bit + 1 + word_bits - 1) / word_bits) * sizeof(unsigned long);
}

static int all_zero(const unsigned char *bits, size_t size) {
    for (size_t i = 0; i < size; ++i)
        if (bits[i]) return 0;
    return 1;
}

/* libevdev queries every capability class and key/LED/switch state even if
 * EVIOCGBIT(0) does not advertise it. Probe every event device so a keyboard
 * cannot hide a mouse/tablet initialization failure. */
static void probe_initialization_bitmaps(void) {
    static const struct { unsigned type, max; } classes[] = {
        { EV_REL, REL_MAX }, { EV_ABS, ABS_MAX }, { EV_LED, LED_MAX },
        { EV_KEY, KEY_MAX }, { EV_SW, SW_MAX }, { EV_MSC, MSC_MAX },
        { EV_FF, FF_MAX }, { EV_SND, SND_MAX },
    };
    static const struct { unsigned type, max, nr; } states[] = {
        { EV_KEY, KEY_MAX, 0x18 }, { EV_LED, LED_MAX, 0x19 },
        { EV_SW, SW_MAX, 0x1b },
    };
    for (int index = 0; index < 32; ++index) {
        char path[32];
        snprintf(path, sizeof(path), "/dev/input/event%d", index);
        int fd = open(path, O_RDONLY | O_CLOEXEC | O_NONBLOCK);
        if (fd < 0) continue;
        int before = failures;
        unsigned char types[sizeof(unsigned long)] = {0};
        if (ioctl(fd, EVIOCGBIT(0, sizeof(types)), types) != (int)sizeof(types)) {
            result("evdev.init_types", "FAIL", errno);
            close(fd);
            continue;
        }
        unsigned char properties[sizeof(unsigned long)] = {0};
        if (ioctl(fd, EVIOCGPROP(sizeof(properties)), properties) < 0)
            result("evdev.init_properties", "FAIL", errno);
        for (size_t i = 0; i < sizeof(classes) / sizeof(classes[0]); ++i) {
            unsigned char bitmap[(KEY_MAX + 1 + 63) / 64 * 8];
            memset(bitmap, 0xa5, sizeof(bitmap));
            size_t size = bitmap_bytes(classes[i].max);
            int count = ioctl(fd, EVIOCGBIT(classes[i].type, size), bitmap);
            int supported = types[classes[i].type / 8] & (1u << (classes[i].type % 8));
            if (count != (int)size || (!supported && !all_zero(bitmap, size))) {
                printf("TK_GRAPHICS kind=evdev.init_bits state=FAIL device=%s type=%u bytes=%d supported=%d errno=%d\n",
                       path, classes[i].type, count, !!supported, count < 0 ? errno : EPROTO);
                failures++;
                continue;
            }
            if (classes[i].type == EV_ABS) {
                for (unsigned axis = 0; axis <= ABS_MAX; ++axis) {
                    if (!(bitmap[axis / 8] & (1u << (axis % 8)))) continue;
                    struct input_absinfo info;
                    if (ioctl(fd, EVIOCGABS(axis), &info) < 0)
                        result("evdev.init_abs", "FAIL", errno);
                }
            }
        }
        for (size_t i = 0; i < sizeof(states) / sizeof(states[0]); ++i) {
            unsigned char bitmap[(KEY_MAX + 1 + 63) / 64 * 8];
            memset(bitmap, 0xa5, sizeof(bitmap));
            size_t size = bitmap_bytes(states[i].max);
            int rc = ioctl(fd, _IOC(_IOC_READ, 'E', states[i].nr, size), bitmap);
            int supported = types[states[i].type / 8] & (1u << (states[i].type % 8));
            if (rc < 0 || (!supported && !all_zero(bitmap, size))) {
                printf("TK_GRAPHICS kind=evdev.init_state state=FAIL device=%s type=%u errno=%d\n",
                       path, states[i].type, rc < 0 ? errno : EPROTO);
                failures++;
            }
        }
        printf("TK_GRAPHICS kind=evdev.init_bitmaps state=%s device=%s\n",
               failures == before ? "OK" : "FAIL", path);
        close(fd);
    }
}

int main(void) {
    printf("TK_GRAPHICS kind=evdev.uapi state=OK input_event=%zu input_id=%zu input_absinfo=%zu version_ioctl=0x%lx id_ioctl=0x%lx bit_ioctl=0x%lx abs_ioctl=0x%lx clockid_ioctl=0x%lx grab_ioctl=0x%lx\n",
           sizeof(struct input_event), sizeof(struct input_id), sizeof(struct input_absinfo),
           (unsigned long)EVIOCGVERSION, (unsigned long)EVIOCGID, (unsigned long)EVIOCGBIT(0, 64), (unsigned long)EVIOCGABS(ABS_X), (unsigned long)EVIOCSCLOCKID, (unsigned long)EVIOCGRAB);
    int fd = open_event();
    if (fd < 0) { result("evdev.open", "FAIL", errno); return 1; }
    int version = 0, clockid = -1, grab = 1;
    struct input_id id; struct input_absinfo abs; unsigned char bits[64]; char bit_text[129];
    memset(&id, 0, sizeof(id)); memset(&abs, 0, sizeof(abs)); memset(bits, 0, sizeof(bits));
    if (ioctl(fd, EVIOCGVERSION, &version) == 0) printf("TK_GRAPHICS kind=evdev.version state=OK value=0x%x\n", version); else result("evdev.version", "FAIL", errno);
    if (ioctl(fd, EVIOCGID, &id) == 0) printf("TK_GRAPHICS kind=evdev.id state=OK bustype=%u vendor=%u product=%u version=%u\n", id.bustype, id.vendor, id.product, id.version); else result("evdev.id", "FAIL", errno);
    int bit_bytes = ioctl(fd, EVIOCGBIT(0, sizeof(bits)), bits);
    if (bit_bytes == (int)sizeof(unsigned long)) {
        bitset_hex(bits, sizeof(bits), bit_text, sizeof(bit_text));
        printf("TK_GRAPHICS kind=evdev.bits state=OK bytes=%d data=%s\n", bit_bytes, bit_text);
        if ((bits[EV_ABS / 8] & (1u << (EV_ABS % 8))) != 0) {
            if (ioctl(fd, EVIOCGABS(ABS_X), &abs) == 0) printf("TK_GRAPHICS kind=evdev.abs_x state=OK min=%d max=%d fuzz=%d flat=%d resolution=%d\n", abs.minimum, abs.maximum, abs.fuzz, abs.flat, abs.resolution);
            else result("evdev.abs_x", "FAIL", errno);
        } else result("evdev.abs_x", "SKIP", ENOTSUP);
    } else result("evdev.bits", "FAIL", bit_bytes < 0 ? errno : EPROTO);
    clockid = CLOCK_MONOTONIC;
    if (ioctl(fd, EVIOCSCLOCKID, &clockid) == 0) {
        int restore_clockid = CLOCK_REALTIME;
        if (ioctl(fd, EVIOCSCLOCKID, &restore_clockid) == 0) printf("TK_GRAPHICS kind=evdev.clockid state=OK\n");
        else result("evdev.clockid_restore", "FAIL", errno);
    } else result("evdev.clockid", "FAIL", errno);
    if (ioctl(fd, EVIOCGRAB, &grab) == 0) { grab = 0; if (ioctl(fd, EVIOCGRAB, &grab) == 0) printf("TK_GRAPHICS kind=evdev.grab state=OK\n"); else result("evdev.grab_release", "FAIL", errno); } else result("evdev.grab", "FAIL", errno);
    close(fd);
    probe_initialization_bitmaps();
    return failures == 0 ? 0 : 1;
}
