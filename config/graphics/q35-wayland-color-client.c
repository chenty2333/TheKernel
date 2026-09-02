#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wayland-client.h>
#include <wayland-egl.h>

#include "q35-xdg-shell-client-protocol.h"

struct app { struct wl_display *d; struct wl_compositor *c; struct xdg_wm_base *wm; struct wl_surface *s; struct xdg_surface *xs; struct xdg_toplevel *top; struct wl_egl_window *win; EGLDisplay ed; EGLSurface es; EGLContext ec; int configured, done, failed; const char *marker; };
static void fail(struct app *a, const char *reason) { if (!a->failed) fprintf(stderr, "%s state=FAIL reason=%s\n", a->marker, reason); a->failed = 1; }
static void ping(void *data, struct xdg_wm_base *wm, uint32_t serial) { (void)data; xdg_wm_base_pong(wm, serial); }
static const struct xdg_wm_base_listener wml = { .ping = ping };
static void configured(void *data, struct xdg_surface *xs, uint32_t serial) { struct app *a = data; xdg_surface_ack_configure(xs, serial); a->configured = 1; }
static const struct xdg_surface_listener xsl = { .configure = configured };
static void topcfg(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *s) { (void)d; (void)t; (void)w; (void)h; (void)s; }
static void close_(void *data, struct xdg_toplevel *top) { (void)top; fail(data, "toplevel_close"); }
static const struct xdg_toplevel_listener tl = { .configure = topcfg, .close = close_ };
static void global(void *data, struct wl_registry *r, uint32_t name, const char *i, uint32_t version) { struct app *a = data; if (!strcmp(i, "wl_compositor")) a->c = wl_registry_bind(r, name, &wl_compositor_interface, version < 4 ? version : 4); else if (!strcmp(i, "xdg_wm_base")) { a->wm = wl_registry_bind(r, name, &xdg_wm_base_interface, 1); xdg_wm_base_add_listener(a->wm, &wml, a); } }
static const struct wl_registry_listener rl = { .global = global };
static void frame_done(void *data, struct wl_callback *cb, uint32_t time) { struct app *a = data; (void)time; wl_callback_destroy(cb); a->done = 1; puts(a->marker); fflush(stdout); }
static const struct wl_callback_listener fl = { .done = frame_done };

static int setup_egl(struct app *a) {
    static const EGLint attrs[] = { EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    static const EGLint ctxattrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLConfig config; EGLint count;
    a->ed = eglGetDisplay((EGLNativeDisplayType)a->d);
    if (a->ed == EGL_NO_DISPLAY || !eglInitialize(a->ed, NULL, NULL) || !eglBindAPI(EGL_OPENGL_ES_API) || !eglChooseConfig(a->ed, attrs, &config, 1, &count) || count != 1) goto bad;
    a->win = wl_egl_window_create(a->s, 800, 600);
    if (!a->win) goto bad;
    a->es = eglCreateWindowSurface(a->ed, config, (EGLNativeWindowType)a->win, NULL);
    a->ec = eglCreateContext(a->ed, config, EGL_NO_CONTEXT, ctxattrs);
    if (a->es == EGL_NO_SURFACE || a->ec == EGL_NO_CONTEXT || !eglMakeCurrent(a->ed, a->es, a->es, a->ec)) goto bad;
    return 0;
bad: fail(a, "egl_initialize"); return -1;
}
static int draw(struct app *a) {
    struct wl_callback *cb;
    glViewport(0, 0, 800, 600); glClearColor(0, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
    glEnable(GL_SCISSOR_TEST); glScissor(300, 200, 200, 200); glClearColor(1, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT); glDisable(GL_SCISSOR_TEST);
    cb = wl_surface_frame(a->s); if (!cb) { fail(a, "frame_callback"); return -1; }
    wl_callback_add_listener(cb, &fl, a);
    if (glGetError() != GL_NO_ERROR || !eglSwapBuffers(a->ed, a->es)) {
        wl_callback_destroy(cb); fail(a, "egl_swap"); return -1;
    }
    return 0;
}
static void cleanup(struct app *a) {
    if (a->ed != EGL_NO_DISPLAY) {
        eglMakeCurrent(a->ed, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        if (a->ec != EGL_NO_CONTEXT) eglDestroyContext(a->ed, a->ec);
        if (a->es != EGL_NO_SURFACE) eglDestroySurface(a->ed, a->es);
        eglTerminate(a->ed);
    }
    if (a->win) wl_egl_window_destroy(a->win);
    if (a->top) xdg_toplevel_destroy(a->top);
    if (a->xs) xdg_surface_destroy(a->xs);
    if (a->s) wl_surface_destroy(a->s);
    if (a->wm) xdg_wm_base_destroy(a->wm);
    if (a->c) wl_compositor_destroy(a->c);
    if (a->d) wl_display_disconnect(a->d);
}
int main(void) {
    const char *marker = getenv("THEKERNEL_Q35_RENDER_MARKER");
    struct app a = { .ed = EGL_NO_DISPLAY, .es = EGL_NO_SURFACE, .ec = EGL_NO_CONTEXT,
        .marker = marker ? marker : "THEKERNEL_Q35_WESTON_READY" };
    struct wl_registry *r; int result = 1;
    a.d = wl_display_connect(NULL); if (!a.d) { fail(&a, "wayland_connect"); goto out; }
    r = wl_display_get_registry(a.d); if (!r) { fail(&a, "wayland_registry"); goto out; }
    wl_registry_add_listener(r, &rl, &a); if (wl_display_roundtrip(a.d) < 0 || !a.c || !a.wm) { fail(&a, "wayland_globals"); goto out; }
    a.s = wl_compositor_create_surface(a.c); if (!a.s) { fail(&a, "wayland_surface"); goto out; }
    a.xs = xdg_wm_base_get_xdg_surface(a.wm, a.s); a.top = xdg_surface_get_toplevel(a.xs); xdg_surface_add_listener(a.xs, &xsl, &a); xdg_toplevel_add_listener(a.top, &tl, &a); xdg_toplevel_set_fullscreen(a.top, NULL); wl_surface_commit(a.s);
    while (!a.configured && !a.failed && wl_display_dispatch(a.d) >= 0) {}
    if (!a.configured || a.failed || setup_egl(&a) || draw(&a)) goto out;
    while (!a.done && !a.failed && wl_display_dispatch(a.d) >= 0) {}
    result = a.done && !a.failed ? 0 : 1;
out: cleanup(&a); return result;
}
