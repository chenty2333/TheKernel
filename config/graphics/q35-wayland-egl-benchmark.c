/*
 * Deterministic 4K EGL/GLES2 workload for the Virgl graphics benchmark.
 *
 * The compositor owns presentation timing, so every new frame is requested
 * only from the preceding wl_surface.frame callback.  This keeps the frame
 * measurements about the complete EGL/Wayland presentation path rather than
 * measuring a client-side busy loop.
 */
#define _POSIX_C_SOURCE 200809L

#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <errno.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include <wayland-egl.h>

#include "q35-xdg-shell-client-protocol.h"

enum {
    WIDTH = 3840,
    HEIGHT = 2160,
    WARMUP_FRAMES = 60,
    FINAL_FRAME = 660,
};

struct app {
    struct wl_display *display;
    struct wl_compositor *compositor;
    struct wl_seat *seat;
    struct wl_pointer *pointer;
    struct wl_keyboard *keyboard;
    struct xdg_wm_base *wm_base;
    struct wl_surface *surface;
    struct xdg_surface *xdg_surface;
    struct xdg_toplevel *toplevel;
    struct wl_egl_window *egl_window;
    struct wl_callback *frame_callback;
    EGLDisplay egl_display;
    EGLSurface egl_surface;
    EGLContext egl_context;
    bool configured;
    bool keyboard_focused;
    bool failed;
    bool done;
    uint32_t frame_index;
    uint32_t abort_after;
    uint64_t previous_frame_ns;
    uint64_t input_ns;
    uint64_t frame_input_ns;
    uint32_t next_input_sequence;
    uint32_t pending_input_sequence;
    uint32_t frame_input_sequence;
    bool input_armed;
    bool input_pending;
    bool frame_has_input;
    bool input_state;
};

static uint64_t monotonic_ns(void)
{
    struct timespec ts;

    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0)
        return 0;
    return (uint64_t)ts.tv_sec * UINT64_C(1000000000) + (uint64_t)ts.tv_nsec;
}

static void fail(struct app *app, const char *reason)
{
    if (!app->failed)
        fprintf(stderr, "THEKERNEL_GRAPHICS_BENCHMARK_READY state=FAIL reason=%s\n", reason);
    app->failed = true;
    app->done = true;
}

static void record_frame_metric(struct app *app)
{
    const uint64_t now = monotonic_ns();

    if (app->previous_frame_ns != 0 && app->frame_index >= WARMUP_FRAMES &&
        app->frame_index < FINAL_FRAME) {
        printf("THEKERNEL_GRAPHICS_METRIC {\"kind\":\"frame\",\"index\":%u,\"ns\":%" PRIu64 "}\n",
               app->frame_index, now - app->previous_frame_ns);
    }
    app->previous_frame_ns = now;
    ++app->frame_index;
    fflush(stdout);
}

static void record_input(struct app *app)
{
    if (app->input_armed && !app->input_pending && !app->frame_has_input) {
        app->pending_input_sequence = app->next_input_sequence;
        ++app->next_input_sequence;
        app->input_pending = true;
        app->input_state = !app->input_state;
        app->input_ns = monotonic_ns();
    }
}

static int schedule_frame(struct app *app);

static void frame_done(void *data, struct wl_callback *callback, uint32_t time)
{
    struct app *app = data;

    (void)time;
    wl_callback_destroy(callback);
    app->frame_callback = NULL;
    record_frame_metric(app);
    if (app->frame_has_input) {
        printf("THEKERNEL_GRAPHICS_METRIC {\"kind\":\"input_to_repaint\",\"ns\":%" PRIu64 "}\n",
               monotonic_ns() - app->frame_input_ns);
        printf("THEKERNEL_GRAPHICS_INPUT_VISIBLE_%03u\n", app->frame_input_sequence);
        fflush(stdout);
        app->frame_has_input = false;
    }

    if (app->abort_after != 0 && app->frame_index >= app->abort_after) {
        abort();
    }
    if (app->frame_index >= FINAL_FRAME) {
        app->done = true;
        return;
    }
    if (schedule_frame(app) != 0)
        fail(app, "egl_swap");
}

static const struct wl_callback_listener frame_listener = {
    .done = frame_done,
};

static int schedule_frame(struct app *app)
{
    const float phase = (float)(app->frame_index % 360U) / 359.0f;
    struct wl_callback *callback;

    if (app->input_pending) {
        app->frame_has_input = true;
        app->frame_input_sequence = app->pending_input_sequence;
        app->frame_input_ns = app->input_ns;
        app->input_pending = false;
    }

    /* Full-frame clear with a deterministic, changing color. */
    glViewport(0, 0, WIDTH, HEIGHT);
    glClearColor(app->input_state ? 0.9f : 0.1f,
                 0.25f + phase * 0.5f,
                 app->input_state ? 0.15f : 0.85f,
                 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    if (glGetError() != GL_NO_ERROR)
        return -1;

    callback = wl_surface_frame(app->surface);
    if (callback == NULL)
        return -1;
    app->frame_callback = callback;
    wl_callback_add_listener(callback, &frame_listener, app);
    if (!eglSwapBuffers(app->egl_display, app->egl_surface)) {
        wl_callback_destroy(callback);
        app->frame_callback = NULL;
        return -1;
    }
    return 0;
}

static void pointer_enter(void *data, struct wl_pointer *pointer, uint32_t serial,
                          struct wl_surface *surface, wl_fixed_t x, wl_fixed_t y)
{
    (void)pointer;
    (void)serial;
    (void)surface;
    (void)x;
    (void)y;
    (void)data;
}

static void pointer_leave(void *data, struct wl_pointer *pointer, uint32_t serial,
                          struct wl_surface *surface)
{
    (void)data;
    (void)pointer;
    (void)serial;
    (void)surface;
}

static void pointer_motion(void *data, struct wl_pointer *pointer, uint32_t time,
                           wl_fixed_t x, wl_fixed_t y)
{
    (void)pointer;
    (void)time;
    (void)x;
    (void)y;
    record_input(data);
}

static void pointer_button(void *data, struct wl_pointer *pointer, uint32_t serial,
                           uint32_t time, uint32_t button, uint32_t state)
{
    (void)pointer;
    (void)serial;
    (void)time;
    (void)button;
    if (state != 0)
        record_input(data);
}

static void pointer_axis(void *data, struct wl_pointer *pointer, uint32_t time,
                         uint32_t axis, wl_fixed_t value)
{
    (void)pointer;
    (void)time;
    (void)axis;
    (void)value;
    (void)data;
}

static const struct wl_pointer_listener pointer_listener = {
    .enter = pointer_enter,
    .leave = pointer_leave,
    .motion = pointer_motion,
    .button = pointer_button,
    .axis = pointer_axis,
};

static void keyboard_keymap(void *data, struct wl_keyboard *keyboard, uint32_t format,
                            int fd, uint32_t size)
{
    (void)data;
    (void)keyboard;
    (void)format;
    (void)size;
    (void)close(fd);
}

static void keyboard_enter(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                           struct wl_surface *surface, struct wl_array *keys)
{
    struct app *app = data;

    (void)keyboard;
    (void)serial;
    (void)surface;
    (void)keys;
    app->keyboard_focused = true;
}

static void keyboard_leave(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                           struct wl_surface *surface)
{
    (void)keyboard;
    (void)serial;
    (void)surface;
    ((struct app *)data)->keyboard_focused = false;
}

static void keyboard_key(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                         uint32_t time, uint32_t key, uint32_t state)
{
    (void)keyboard;
    (void)serial;
    (void)time;
    (void)key;
    if (state != 0)
        record_input(data);
}

static void keyboard_modifiers(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                               uint32_t depressed, uint32_t latched, uint32_t locked,
                               uint32_t group)
{
    (void)data;
    (void)keyboard;
    (void)serial;
    (void)depressed;
    (void)latched;
    (void)locked;
    (void)group;
}

static void keyboard_repeat_info(void *data, struct wl_keyboard *keyboard, int32_t rate,
                                 int32_t delay)
{
    (void)data;
    (void)keyboard;
    (void)rate;
    (void)delay;
}

static const struct wl_keyboard_listener keyboard_listener = {
    .keymap = keyboard_keymap,
    .enter = keyboard_enter,
    .leave = keyboard_leave,
    .key = keyboard_key,
    .modifiers = keyboard_modifiers,
    .repeat_info = keyboard_repeat_info,
};

static void seat_capabilities(void *data, struct wl_seat *seat, uint32_t capabilities)
{
    struct app *app = data;

    if ((capabilities & WL_SEAT_CAPABILITY_POINTER) != 0 && app->pointer == NULL) {
        app->pointer = wl_seat_get_pointer(seat);
        if (app->pointer != NULL)
            wl_pointer_add_listener(app->pointer, &pointer_listener, app);
    }
    if ((capabilities & WL_SEAT_CAPABILITY_KEYBOARD) != 0 && app->keyboard == NULL) {
        app->keyboard = wl_seat_get_keyboard(seat);
        if (app->keyboard != NULL)
            wl_keyboard_add_listener(app->keyboard, &keyboard_listener, app);
    }
}

static void seat_name(void *data, struct wl_seat *seat, const char *name)
{
    (void)data;
    (void)seat;
    (void)name;
}

static const struct wl_seat_listener seat_listener = {
    .capabilities = seat_capabilities,
    .name = seat_name,
};

static void wm_base_ping(void *data, struct xdg_wm_base *wm_base, uint32_t serial)
{
    (void)data;
    xdg_wm_base_pong(wm_base, serial);
}

static const struct xdg_wm_base_listener wm_base_listener = {
    .ping = wm_base_ping,
};

static void xdg_surface_configure(void *data, struct xdg_surface *surface, uint32_t serial)
{
    struct app *app = data;

    xdg_surface_ack_configure(surface, serial);
    app->configured = true;
}

static const struct xdg_surface_listener xdg_surface_listener = {
    .configure = xdg_surface_configure,
};

static void toplevel_configure(void *data, struct xdg_toplevel *toplevel, int32_t width,
                               int32_t height, struct wl_array *states)
{
    (void)data;
    (void)toplevel;
    (void)width;
    (void)height;
    (void)states;
}

static void toplevel_close(void *data, struct xdg_toplevel *toplevel)
{
    (void)toplevel;
    fail(data, "toplevel_close");
}

static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure,
    .close = toplevel_close,
};

static void registry_global(void *data, struct wl_registry *registry, uint32_t name,
                            const char *interface, uint32_t version)
{
    struct app *app = data;

    if (strcmp(interface, wl_compositor_interface.name) == 0) {
        app->compositor = wl_registry_bind(registry, name, &wl_compositor_interface,
                                           version < 4 ? version : 4);
    } else if (strcmp(interface, wl_seat_interface.name) == 0) {
        app->seat = wl_registry_bind(registry, name, &wl_seat_interface,
                                     version < 5 ? version : 5);
        if (app->seat != NULL)
            wl_seat_add_listener(app->seat, &seat_listener, app);
    } else if (strcmp(interface, xdg_wm_base_interface.name) == 0) {
        app->wm_base = wl_registry_bind(registry, name, &xdg_wm_base_interface, 1);
        if (app->wm_base != NULL)
            xdg_wm_base_add_listener(app->wm_base, &wm_base_listener, app);
    }
}

static void registry_global_remove(void *data, struct wl_registry *registry, uint32_t name)
{
    (void)data;
    (void)registry;
    (void)name;
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_global_remove,
};

static int setup_egl(struct app *app)
{
    static const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8,
        EGL_GREEN_SIZE, 8,
        EGL_BLUE_SIZE, 8,
        EGL_NONE,
    };
    static const EGLint context_attributes[] = {
        EGL_CONTEXT_CLIENT_VERSION, 2,
        EGL_NONE,
    };
    EGLConfig config;
    EGLint count = 0;

    app->egl_display = eglGetDisplay((EGLNativeDisplayType)app->display);
    if (app->egl_display == EGL_NO_DISPLAY ||
        !eglInitialize(app->egl_display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_ES_API) ||
        !eglChooseConfig(app->egl_display, config_attributes, &config, 1, &count) ||
        count != 1) {
        return -1;
    }
    app->egl_window = wl_egl_window_create(app->surface, WIDTH, HEIGHT);
    if (app->egl_window == NULL)
        return -1;
    app->egl_surface = eglCreateWindowSurface(app->egl_display, config,
                                              (EGLNativeWindowType)app->egl_window, NULL);
    app->egl_context = eglCreateContext(app->egl_display, config, EGL_NO_CONTEXT,
                                        context_attributes);
    if (app->egl_surface == EGL_NO_SURFACE || app->egl_context == EGL_NO_CONTEXT ||
        !eglMakeCurrent(app->egl_display, app->egl_surface, app->egl_surface,
                        app->egl_context)) {
        return -1;
    }
    return 0;
}

static uint32_t parse_u32(const char *value)
{
    char *end = NULL;
    unsigned long parsed;

    if (value == NULL || *value == '\0')
        return 0;
    errno = 0;
    parsed = strtoul(value, &end, 10);
    if (errno != 0 || end == value || *end != '\0' || parsed > UINT32_MAX)
        return 0;
    return (uint32_t)parsed;
}

static void cleanup(struct app *app)
{
    if (app->frame_callback != NULL)
        wl_callback_destroy(app->frame_callback);
    if (app->egl_display != EGL_NO_DISPLAY) {
        (void)eglMakeCurrent(app->egl_display, EGL_NO_SURFACE, EGL_NO_SURFACE,
                             EGL_NO_CONTEXT);
        if (app->egl_context != EGL_NO_CONTEXT)
            (void)eglDestroyContext(app->egl_display, app->egl_context);
        if (app->egl_surface != EGL_NO_SURFACE)
            (void)eglDestroySurface(app->egl_display, app->egl_surface);
        (void)eglTerminate(app->egl_display);
    }
    if (app->egl_window != NULL)
        wl_egl_window_destroy(app->egl_window);
    if (app->toplevel != NULL)
        xdg_toplevel_destroy(app->toplevel);
    if (app->xdg_surface != NULL)
        xdg_surface_destroy(app->xdg_surface);
    if (app->surface != NULL)
        wl_surface_destroy(app->surface);
    if (app->keyboard != NULL)
        wl_keyboard_destroy(app->keyboard);
    if (app->pointer != NULL)
        wl_pointer_destroy(app->pointer);
    if (app->seat != NULL)
        wl_seat_destroy(app->seat);
    if (app->wm_base != NULL)
        xdg_wm_base_destroy(app->wm_base);
    if (app->compositor != NULL)
        wl_compositor_destroy(app->compositor);
    if (app->display != NULL)
        wl_display_disconnect(app->display);
}

int main(void)
{
    struct app app = {
        .egl_display = EGL_NO_DISPLAY,
        .egl_surface = EGL_NO_SURFACE,
        .egl_context = EGL_NO_CONTEXT,
        .abort_after = parse_u32(getenv("THEKERNEL_GRAPHICS_ABORT_AFTER")),
    };
    struct wl_registry *registry;
    int result = EXIT_FAILURE;

    app.display = wl_display_connect(NULL);
    if (app.display == NULL) {
        fail(&app, "wayland_connect");
        goto out;
    }
    registry = wl_display_get_registry(app.display);
    if (registry == NULL) {
        fail(&app, "wayland_registry");
        goto out;
    }
    wl_registry_add_listener(registry, &registry_listener, &app);
    if (wl_display_roundtrip(app.display) < 0 || app.compositor == NULL ||
        app.wm_base == NULL || app.seat == NULL) {
        fail(&app, "wayland_globals");
        goto out;
    }
    app.surface = wl_compositor_create_surface(app.compositor);
    if (app.surface == NULL) {
        fail(&app, "wayland_surface");
        goto out;
    }
    app.xdg_surface = xdg_wm_base_get_xdg_surface(app.wm_base, app.surface);
    app.toplevel = xdg_surface_get_toplevel(app.xdg_surface);
    if (app.xdg_surface == NULL || app.toplevel == NULL) {
        fail(&app, "xdg_surface");
        goto out;
    }
    xdg_surface_add_listener(app.xdg_surface, &xdg_surface_listener, &app);
    xdg_toplevel_add_listener(app.toplevel, &toplevel_listener, &app);
    xdg_toplevel_set_title(app.toplevel, "TheKernel 4K EGL benchmark");
    xdg_toplevel_set_fullscreen(app.toplevel, NULL);
    wl_surface_commit(app.surface);
    while (!app.configured && !app.failed && wl_display_dispatch(app.display) >= 0)
        ;
    if (!app.configured || app.failed) {
        fail(&app, "wayland_configure");
        goto out;
    }
    /* Seat capability events and initial keyboard focus are delivered here. */
    if (wl_display_roundtrip(app.display) < 0 || app.pointer == NULL ||
        app.keyboard == NULL || !app.keyboard_focused) {
        fail(&app, "input_focus_roundtrip");
        goto out;
    }
    if (setup_egl(&app) != 0) {
        fail(&app, "egl_initialize");
        goto out;
    }
    puts("THEKERNEL_GRAPHICS_BENCHMARK_READY");
    fflush(stdout);
    app.input_armed = true;
    if (schedule_frame(&app) != 0) {
        fail(&app, "egl_swap");
        goto out;
    }
    while (!app.done && !app.failed && wl_display_dispatch(app.display) >= 0)
        ;
    result = app.done && !app.failed ? EXIT_SUCCESS : EXIT_FAILURE;
out:
    cleanup(&app);
    return result;
}
