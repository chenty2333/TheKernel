#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <time.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <wayland-client.h>

#include "q35-xdg-shell-client-protocol.h"

#ifndef THEKERNEL_WIDTH
#define THEKERNEL_WIDTH 800
#endif
#ifndef THEKERNEL_HEIGHT
#define THEKERNEL_HEIGHT 600
#endif
enum { WIDTH = THEKERNEL_WIDTH, HEIGHT = THEKERNEL_HEIGHT, STRIDE = WIDTH * 4 };
enum { BUFFER_COUNT = 3 };
struct app {
    struct wl_display *display; struct wl_compositor *compositor; struct wl_shm *shm;
    struct wl_seat *seat; struct wl_pointer *pointer; struct wl_keyboard *keyboard;
    struct xdg_wm_base *wm; struct wl_surface *surface; struct xdg_surface *xdg_surface;
    struct xdg_toplevel *toplevel; struct wl_buffer *buffer; uint32_t *pixels;
    struct wl_buffer *buffers[BUFFER_COUNT]; uint32_t *buffer_pixels[BUFFER_COUNT];
    int buffer_busy[BUFFER_COUNT]; unsigned int next_buffer;
    int configured, keyboard_focused; const char *checkpoint;
    int benchmark, done, input_armed, input_pending, frame_has_input;
    uint32_t frame_index, abort_after, input_sequence, frame_input_sequence, benchmark_accent;
    uint64_t previous_frame_ns, input_ns, frame_input_ns;
};
static uint64_t monotonic_ns(void) { struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts); return (uint64_t)ts.tv_sec * 1000000000ULL + ts.tv_nsec; }
static void repaint(struct app *a, uint32_t accent, const char *checkpoint);
static void metric_frame(struct app *a) {
    uint64_t now = monotonic_ns();
    if (a->previous_frame_ns && a->frame_index >= 60 && a->frame_index < 660)
        printf("THEKERNEL_GRAPHICS_METRIC {\"kind\":\"frame\",\"index\":%u,\"ns\":%llu}\n", a->frame_index, (unsigned long long)(now - a->previous_frame_ns));
    a->previous_frame_ns = now; ++a->frame_index; fflush(stdout);
}
static void benchmark_input(struct app *a) {
    if (!a->benchmark || !a->input_armed || a->input_pending || a->frame_has_input) return;
    a->input_ns = monotonic_ns();
    a->input_pending = 1;
    a->benchmark_accent = (a->input_sequence & 1) ? 0xff00cc66 : 0xffff00ff;
}
static void fail(const char *reason) { fprintf(stderr, "THEKERNEL_Q35_WAYLAND_READY state=FAIL reason=%s\n", reason); }
static void paint(struct app *a, uint32_t accent) {
    if (a->benchmark) {
        /* Exactly 10% of a full-screen 4K surface: damage is bounded rather
         * than accidentally benchmarking an unconditional full-frame copy. */
        for (int y = 0; y < HEIGHT / 10; ++y) for (int x = 0; x < WIDTH; ++x)
            a->pixels[y * WIDTH + x] = accent;
        return;
    }
    for (int y = 0; y < HEIGHT; ++y) for (int x = 0; x < WIDTH; ++x)
        a->pixels[y * WIDTH + x] = ((x / 32 + y / 32) & 1) ? 0xff101010 : 0xff202020;
    for (int y = 200; y < 400; ++y) for (int x = 300; x < 500; ++x) a->pixels[y * WIDTH + x] = accent;
}
static void frame_done(void *data, struct wl_callback *callback, uint32_t time) {
    struct app *a = data; (void)time; wl_callback_destroy(callback);
    if (a->benchmark) {
        metric_frame(a);
        if (a->frame_has_input) {
            uint64_t now = monotonic_ns();
            printf("THEKERNEL_GRAPHICS_METRIC {\"kind\":\"input_to_repaint\",\"ns\":%llu}\n",
                   (unsigned long long)(now - a->frame_input_ns));
            printf("THEKERNEL_GRAPHICS_INPUT_VISIBLE_%03u\n", a->frame_input_sequence);
            ++a->input_sequence;
            a->frame_has_input = 0;
            fflush(stdout);
        }
        if (a->abort_after && a->frame_index >= a->abort_after) abort();
        if (a->frame_index < 660) { if (!a->input_pending) a->benchmark_accent = (a->frame_index & 1) ? 0xffff2020 : 0xff2060ff; repaint(a, a->benchmark_accent, NULL); return; }
        a->done = 1; return;
    }
    if (a->checkpoint) { puts(a->checkpoint); fflush(stdout); a->checkpoint = NULL; }
}
static const struct wl_callback_listener frame_listener = { .done = frame_done };
static void buffer_release(void *data, struct wl_buffer *buffer) {
    struct app *a = data;
    for (unsigned int index = 0; index < BUFFER_COUNT; ++index) {
        if (a->buffers[index] == buffer) { a->buffer_busy[index] = 0; return; }
    }
}
static const struct wl_buffer_listener buffer_listener = { .release = buffer_release };
static void repaint(struct app *a, uint32_t accent, const char *checkpoint) {
    unsigned int selected = BUFFER_COUNT;
    for (unsigned int offset = 0; offset < BUFFER_COUNT; ++offset) {
        unsigned int index = (a->next_buffer + offset) % BUFFER_COUNT;
        if (!a->buffer_busy[index]) { selected = index; break; }
    }
    if (selected == BUFFER_COUNT) { fail("shm_buffers_busy"); a->done = 1; return; }
    a->next_buffer = (selected + 1) % BUFFER_COUNT;
    a->buffer = a->buffers[selected];
    a->pixels = a->buffer_pixels[selected];
    a->buffer_busy[selected] = 1;
    if (a->benchmark && a->input_pending) {
        a->frame_has_input = 1;
        a->frame_input_sequence = a->input_sequence;
        a->frame_input_ns = a->input_ns;
        a->input_pending = 0;
    }
    paint(a, accent); a->checkpoint = checkpoint; wl_surface_attach(a->surface, a->buffer, 0, 0);
    if (a->benchmark) wl_surface_damage_buffer(a->surface, 0, 0, WIDTH, HEIGHT / 10);
    else wl_surface_damage_buffer(a->surface, 0, 0, WIDTH, HEIGHT);
    struct wl_callback *callback = wl_surface_frame(a->surface); wl_callback_add_listener(callback, &frame_listener, a); wl_surface_commit(a->surface);
}
static void pointer_enter(void *data, struct wl_pointer *p, uint32_t s, struct wl_surface *surface, wl_fixed_t x, wl_fixed_t y) { struct app *a = data; (void)p; (void)s; (void)surface; (void)x; (void)y; if (a->benchmark) benchmark_input(a); else { a->input_ns = monotonic_ns(); repaint(a, 0xff0066ff, "THEKERNEL_Q35_WAYLAND_POINTER_REPAINT"); } }
static void pointer_leave(void *d, struct wl_pointer *p, uint32_t s, struct wl_surface *surface) { (void)d; (void)p; (void)s; (void)surface; }
static void pointer_motion(void *data, struct wl_pointer *p, uint32_t t, wl_fixed_t x, wl_fixed_t y) { struct app *a = data; (void)p; (void)t; (void)x; (void)y; if (a->benchmark) benchmark_input(a); else { a->input_ns = monotonic_ns(); repaint(a, 0xff0066ff, "THEKERNEL_Q35_WAYLAND_POINTER_REPAINT"); } }
static void pointer_button(void *data, struct wl_pointer *p, uint32_t s, uint32_t t, uint32_t b, uint32_t state) { struct app *a = data; (void)p; (void)s; (void)t; (void)b; if (state) { if (a->benchmark) benchmark_input(a); else { a->input_ns = monotonic_ns(); repaint(a, 0xff00cc66, "THEKERNEL_Q35_WAYLAND_BUTTON_REPAINT"); } } }
static void pointer_axis(void *data, struct wl_pointer *p, uint32_t t, uint32_t axis, wl_fixed_t value) { struct app *a = data; (void)p; (void)t; (void)axis; (void)value; if (a->benchmark) benchmark_input(a); else repaint(a, 0xffffff00, "THEKERNEL_Q35_WAYLAND_SCROLL_REPAINT"); }
static const struct wl_pointer_listener pointer_listener = { .enter = pointer_enter, .leave = pointer_leave, .motion = pointer_motion, .button = pointer_button, .axis = pointer_axis };
static void keymap(void *d, struct wl_keyboard *k, uint32_t f, int fd, uint32_t size) { (void)d; (void)k; (void)f; (void)size; close(fd); }
static void keyboard_enter(void *data, struct wl_keyboard *k, uint32_t s, struct wl_surface *surface, struct wl_array *keys) { struct app *a = data; (void)k; (void)s; (void)surface; (void)keys; a->keyboard_focused = 1; }
static void keyboard_leave(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *surface) { (void)d; (void)k; (void)s; (void)surface; }
static void keyboard_key(void *data, struct wl_keyboard *k, uint32_t s, uint32_t t, uint32_t key, uint32_t state) { struct app *a = data; (void)k; (void)s; (void)t; (void)key; if (state) { if (a->benchmark) benchmark_input(a); else { a->input_ns = monotonic_ns(); repaint(a, 0xffff00ff, "THEKERNEL_Q35_WAYLAND_KEY_REPAINT"); } } }
static void keyboard_modifiers(void *d, struct wl_keyboard *k, uint32_t s, uint32_t a, uint32_t b, uint32_t c, uint32_t group) { (void)d; (void)k; (void)s; (void)a; (void)b; (void)c; (void)group; }
static void keyboard_repeat(void *d, struct wl_keyboard *k, int32_t rate, int32_t delay) { (void)d; (void)k; (void)rate; (void)delay; }
static const struct wl_keyboard_listener keyboard_listener = { .keymap = keymap, .enter = keyboard_enter, .leave = keyboard_leave, .key = keyboard_key, .modifiers = keyboard_modifiers, .repeat_info = keyboard_repeat };
static void seat_caps(void *data, struct wl_seat *seat, uint32_t caps) { struct app *a = data; if ((caps & WL_SEAT_CAPABILITY_POINTER) && !a->pointer) { a->pointer = wl_seat_get_pointer(seat); wl_pointer_add_listener(a->pointer, &pointer_listener, a); } if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !a->keyboard) { a->keyboard = wl_seat_get_keyboard(seat); wl_keyboard_add_listener(a->keyboard, &keyboard_listener, a); } }
static void seat_name(void *d, struct wl_seat *seat, const char *name) { (void)d; (void)seat; (void)name; }
static const struct wl_seat_listener seat_listener = { .capabilities = seat_caps, .name = seat_name };
static void ping(void *d, struct xdg_wm_base *wm, uint32_t serial) { (void)d; xdg_wm_base_pong(wm, serial); }
static const struct xdg_wm_base_listener wm_listener = { .ping = ping };
static void configure(void *data, struct xdg_surface *surface, uint32_t serial) { struct app *a = data; xdg_surface_ack_configure(surface, serial); a->configured = 1; }
static const struct xdg_surface_listener xdg_surface_listener = { .configure = configure };
static void top_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *states) { (void)d; (void)t; (void)w; (void)h; (void)states; }
static void top_close(void *d, struct xdg_toplevel *t) { (void)d; (void)t; }
static const struct xdg_toplevel_listener top_listener = { .configure = top_configure, .close = top_close };
static void global(void *data, struct wl_registry *r, uint32_t name, const char *interface, uint32_t version) { struct app *a = data; if (!strcmp(interface, "wl_compositor")) a->compositor = wl_registry_bind(r, name, &wl_compositor_interface, version < 4 ? version : 4); else if (!strcmp(interface, "wl_shm")) a->shm = wl_registry_bind(r, name, &wl_shm_interface, 1); else if (!strcmp(interface, "xdg_wm_base")) { a->wm = wl_registry_bind(r, name, &xdg_wm_base_interface, 1); xdg_wm_base_add_listener(a->wm, &wm_listener, a); } else if (!strcmp(interface, "wl_seat")) { a->seat = wl_registry_bind(r, name, &wl_seat_interface, version < 5 ? version : 5); wl_seat_add_listener(a->seat, &seat_listener, a); } }
static const struct wl_registry_listener registry_listener = { .global = global };
static int create_buffer(struct app *a) { char name[] = "/thekernel-wl-shm-XXXXXX"; int fd = mkstemp(name); if (fd < 0) return -1; unlink(name); size_t bytes = (size_t)STRIDE * HEIGHT; size_t total = bytes * BUFFER_COUNT; if (ftruncate(fd, (off_t)total) < 0) { close(fd); return -1; } uint32_t *mapping = mmap(NULL, total, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0); if (mapping == MAP_FAILED) { close(fd); return -1; } struct wl_shm_pool *pool = wl_shm_create_pool(a->shm, fd, (int)total); close(fd); for (unsigned int index = 0; index < BUFFER_COUNT; ++index) { a->buffer_pixels[index] = mapping + (bytes / sizeof(*mapping)) * index; a->buffers[index] = wl_shm_pool_create_buffer(pool, (int)(bytes * index), WIDTH, HEIGHT, STRIDE, WL_SHM_FORMAT_XRGB8888); if (!a->buffers[index]) { wl_shm_pool_destroy(pool); return -1; } wl_buffer_add_listener(a->buffers[index], &buffer_listener, a); } wl_shm_pool_destroy(pool); return 0; }
int main(void) { struct app a = { .benchmark = getenv("THEKERNEL_GRAPHICS_BENCHMARK") != NULL, .benchmark_accent = 0xffff0000 }; const char *abort_after = getenv("THEKERNEL_GRAPHICS_ABORT_AFTER"); if (abort_after) a.abort_after = strtoul(abort_after, NULL, 10); a.display = wl_display_connect(NULL); if (!a.display) { fail("wayland_connect"); return 1; } struct wl_registry *registry = wl_display_get_registry(a.display); wl_registry_add_listener(registry, &registry_listener, &a); if (wl_display_roundtrip(a.display) < 0 || !a.compositor || !a.shm || !a.wm || !a.seat) { fail("wayland_globals"); return 1; } a.surface = wl_compositor_create_surface(a.compositor); a.xdg_surface = xdg_wm_base_get_xdg_surface(a.wm, a.surface); a.toplevel = xdg_surface_get_toplevel(a.xdg_surface); xdg_surface_add_listener(a.xdg_surface, &xdg_surface_listener, &a); xdg_toplevel_add_listener(a.toplevel, &top_listener, &a); xdg_toplevel_set_title(a.toplevel, "TheKernel deterministic SHM"); xdg_toplevel_set_fullscreen(a.toplevel, NULL); wl_surface_commit(a.surface); while (!a.configured && wl_display_dispatch(a.display) >= 0) {} if (!a.configured || create_buffer(&a)) { fail("shm_buffer"); return 1; } if (a.benchmark) { if (!a.pointer || !a.keyboard || wl_display_roundtrip(a.display) < 0 || !a.keyboard_focused) { fail("input_focus_roundtrip"); return 1; } puts("THEKERNEL_GRAPHICS_BENCHMARK_READY"); fflush(stdout); a.input_armed = 1; } repaint(&a, a.benchmark_accent, a.benchmark ? "THEKERNEL_Q35_WAYLAND_REPAINT_READY" : "THEKERNEL_Q35_WESTON_READY"); while (wl_display_dispatch(a.display) >= 0) { if (a.benchmark && a.done) break; if (!a.benchmark && !a.checkpoint) { puts("THEKERNEL_Q35_WAYLAND_INPUT_READY"); fflush(stdout); break; } } while (!a.benchmark && wl_display_dispatch(a.display) >= 0) {} return 0; }
