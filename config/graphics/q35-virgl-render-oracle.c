#define _GNU_SOURCE

/*
 * Exercise the render-node part of the legacy VirtGPU contract directly.
 * Mesa owns command-stream construction in the EGL workload. A small 3D
 * resource exercises PRIME export/import. Keeping the ABI probe to discovery,
 * capability, PRIME and sync_file primitives makes an unsupported render
 * node fail before Mesa can silently select llvmpipe.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <drm.h>
#include <drm_mode.h>
#include <virtgpu_drm.h>

static int fail(const char *reason)
{
    fprintf(stderr, "THEKERNEL_Q35_VIRGL_RENDER_READY state=FAIL reason=%s errno=%d\\n",
            reason, errno);
    return 1;
}

static int getparam(int fd, uint64_t param, uint64_t *value)
{
    struct drm_virtgpu_getparam request = {
        .param = param,
        .value = (uintptr_t)value,
    };

    if (ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &request) < 0)
        return -1;
    return 0;
}

enum capability_status {
    CAPABILITY_ERROR = -1,
    CAPABILITY_UNSUPPORTED = 0,
    CAPABILITY_SUPPORTED = 1,
};

struct capability_result {
    enum capability_status status;
    const char *reason;
};

/* A non-VirtIO render node does not implement these private ioctls. */
static int unsupported_getparam_error(void)
{
    return errno == ENOTTY || errno == EINVAL || errno == EOPNOTSUPP;
}

static int has_capset(uint64_t capsets, uint32_t capset_id)
{
    return capset_id < 64 && (capsets & (UINT64_C(1) << capset_id)) != 0;
}

static int probe_capset(int fd, uint32_t capset_id)
{
    uint8_t caps[4096] = { 0 };
    struct drm_virtgpu_get_caps request = {
        .cap_set_id = capset_id,
        /* Mesa requests version 0 for Virgl, Virgl2, and Venus capsets. */
        .cap_set_ver = 0,
        .addr = (uintptr_t)caps,
        .size = sizeof(caps),
    };

    return ioctl(fd, DRM_IOCTL_VIRTGPU_GET_CAPS, &request);
}

/* DRIVER_RENDER is advertised statically, so only the VirtGPU GETPARAMs and
 * a working capset query distinguish a Virgl renderer from a software-only
 * render node. This is deliberately separate from the strict KMS/PRIME/sync
 * conformance oracle below so it can safely select the Pixman fallback.
 */
static struct capability_result probe_virgl_capability(int fd)
{
    uint64_t value = 0;
    if (getparam(fd, VIRTGPU_PARAM_3D_FEATURES, &value) < 0) {
        if (unsupported_getparam_error())
            return (struct capability_result) { CAPABILITY_UNSUPPORTED, "getparam_3d" };
        return (struct capability_result) { CAPABILITY_ERROR, "getparam_3d" };
    }
    if (value == 0)
        return (struct capability_result) { CAPABILITY_UNSUPPORTED, "getparam_3d" };

    uint64_t capsets = 0;
    if (getparam(fd, VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs, &capsets) < 0) {
        if (unsupported_getparam_error())
            return (struct capability_result) { CAPABILITY_UNSUPPORTED, "getparam_capsets" };
        return (struct capability_result) { CAPABILITY_ERROR, "getparam_capsets" };
    }

    uint32_t capset_id;
    if (has_capset(capsets, VIRTGPU_DRM_CAPSET_VIRGL))
        capset_id = VIRTGPU_DRM_CAPSET_VIRGL;
    else if (has_capset(capsets, VIRTGPU_DRM_CAPSET_VIRGL2))
        capset_id = VIRTGPU_DRM_CAPSET_VIRGL2;
    else
        return (struct capability_result) { CAPABILITY_UNSUPPORTED, "getparam_capsets" };

    /* A capset advertised by GETPARAM but rejected by GET_CAPS is a broken
     * query path, not absence of a renderer: surface it as an error instead
     * of silently routing around it through Pixman.
     */
    if (probe_capset(fd, capset_id) < 0)
        return (struct capability_result) { CAPABILITY_ERROR, "get_caps_virgl" };
    return (struct capability_result) { CAPABILITY_SUPPORTED, NULL };
}

/* Venus is a Vulkan capset, not the Virgl Gallium renderer. It is queried
 * separately because some Venus-only devices do not advertise the legacy
 * VIRTGPU_PARAM_3D_FEATURES bit used for Virgl selection.
 */
static struct capability_result probe_venus_capability(int fd)
{
    uint64_t capsets = 0;
    if (getparam(fd, VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs, &capsets) < 0) {
        if (unsupported_getparam_error())
            return (struct capability_result) { CAPABILITY_UNSUPPORTED, "getparam_capsets" };
        return (struct capability_result) { CAPABILITY_ERROR, "getparam_capsets" };
    }
    if (!has_capset(capsets, VIRTGPU_DRM_CAPSET_VENUS))
        return (struct capability_result) { CAPABILITY_UNSUPPORTED, "getparam_capsets" };
    /* Mesa's Venus renderer explicitly initializes this version to zero. */
    if (probe_capset(fd, VIRTGPU_DRM_CAPSET_VENUS) < 0)
        return (struct capability_result) { CAPABILITY_ERROR, "get_caps_venus" };
    return (struct capability_result) { CAPABILITY_SUPPORTED, NULL };
}

static int probe_prime(int fd)
{
    struct drm_get_cap request = { .capability = DRM_CAP_PRIME };
    if (ioctl(fd, DRM_IOCTL_GET_CAP, &request) < 0)
        return -1;
    if ((request.value & (DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT)) !=
        (DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT)) {
        errno = EOPNOTSUPP;
        return -1;
    }
    struct drm_virtgpu_resource_create resource = {
        .target = 2, /* PIPE_TEXTURE_2D */
        .format = 1, /* PIPE_FORMAT_B8G8R8A8_UNORM */
        .bind = 2, /* VIRGL_BIND_RENDER_TARGET */
        .width = 16, .height = 16, .depth = 1, .array_size = 1,
        .size = 4096, .stride = 64,
    };
    struct drm_prime_handle exported = { .fd = -1 };
    struct drm_prime_handle imported = { 0 };
    int result = -1;
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, &resource) < 0)
        return -1;
    exported.handle = resource.bo_handle;
    /* Literal x86_64 Linux O_CLOEXEC | O_RDWR guards the flag ABI. */
    exported.flags = 0x80002;
    if (ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &exported) < 0)
        goto prime_destroy;
    int descriptor_flags = fcntl(exported.fd, F_GETFD);
    if (descriptor_flags < 0)
        goto prime_destroy;
    if (!(descriptor_flags & FD_CLOEXEC)) {
        errno = EIO;
        goto prime_destroy;
    }
    imported.fd = exported.fd;
    if (ioctl(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &imported) < 0)
        goto prime_destroy;
    if (!imported.handle) {
        errno = EIO;
        goto prime_destroy;
    }
    result = 0;
prime_destroy:;
    int saved_errno = errno;
    if (imported.handle && imported.handle != resource.bo_handle)
        (void)ioctl(fd, DRM_IOCTL_GEM_CLOSE, &(struct drm_gem_close) {
            .handle = imported.handle });
    if (exported.fd >= 0)
        close(exported.fd);
    (void)ioctl(fd, DRM_IOCTL_GEM_CLOSE, &(struct drm_gem_close) {
        .handle = resource.bo_handle });
    errno = saved_errno;
    return result;
}

static int probe_sync_file(int fd)
{
    struct drm_syncobj_create create = { .flags = DRM_SYNCOBJ_CREATE_SIGNALED };
    struct drm_syncobj_create destination = { 0 };
    struct drm_syncobj_handle export_fd = { .fd = -1 };
    struct drm_syncobj_handle imported_fd = { .fd = -1 };
    int result = -1;

    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &create) < 0)
        return -1;
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &destination) < 0)
        goto destroy;
    export_fd.handle = create.handle;
    export_fd.flags = DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE;
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &export_fd) < 0)
        goto destroy;
    /* IMPORT_SYNC_FILE replaces the fence in an existing destination. */
    struct drm_syncobj_handle import_fd = {
        .handle = destination.handle,
        .fd = export_fd.fd,
        .flags = DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE,
    };
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &import_fd) < 0)
        goto destroy;
    imported_fd.handle = destination.handle;
    imported_fd.flags = DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE;
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &imported_fd) < 0)
        goto destroy;
    struct pollfd ready = { .fd = imported_fd.fd, .events = POLLIN };
    if (poll(&ready, 1, 0) != 1 || !(ready.revents & POLLIN) ||
        (ready.revents & (POLLERR | POLLNVAL))) {
        errno = EIO;
        goto destroy;
    }
    result = 0;

destroy:;
    int saved_errno = errno;
    if (imported_fd.fd >= 0)
        close(imported_fd.fd);
    if (export_fd.fd >= 0)
        close(export_fd.fd);
    if (destination.handle)
        (void)ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &(struct drm_syncobj_destroy) {
            .handle = destination.handle });
    (void)ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &(struct drm_syncobj_destroy) {
        .handle = create.handle });
    errno = saved_errno;
    return result;
}

static int probe_only(const char *name, struct capability_result result)
{
    if (result.status == CAPABILITY_SUPPORTED)
        return 0;
    if (result.status == CAPABILITY_UNSUPPORTED)
        return 1;
    fprintf(stderr, "%s capability probe failed: errno=%d\n", name, errno);
    return 2;
}

int main(int argc, char **argv)
{
    int supports_virgl = argc == 2 && strcmp(argv[1], "--supports-virgl") == 0;
    int supports_venus = argc == 2 && strcmp(argv[1], "--supports-venus") == 0;

    if (argc != 1 && !supports_virgl && !supports_venus) {
        fprintf(stderr, "usage: %s [--supports-virgl|--supports-venus]\n", argv[0]);
        return 2;
    }

    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);

    if (supports_virgl || supports_venus) {
        if (fd < 0) {
            if (errno == ENOENT || errno == ENODEV)
                return 1;
            fprintf(stderr, "render-node capability probe failed: errno=%d\n", errno);
            return 2;
        }
        struct capability_result capability = supports_virgl ?
            probe_virgl_capability(fd) : probe_venus_capability(fd);
        int status = probe_only(supports_virgl ? "Virgl" : "Venus", capability);
        close(fd);
        return status;
    }
    if (fd < 0)
        return fail("render_node");
    struct drm_mode_card_res kms = { 0 };
    errno = 0;
    /* Linux drm_ioctl_permit rejects non-DRM_RENDER_ALLOW ioctls with EACCES. */
    if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &kms) == 0 || errno != EACCES) {
        close(fd);
        return fail("render_node_kms_isolation");
    }
    struct capability_result capability = probe_virgl_capability(fd);
    if (capability.status != CAPABILITY_SUPPORTED) {
        if (capability.status == CAPABILITY_UNSUPPORTED)
            errno = EOPNOTSUPP;
        const char *reason = capability.reason ? capability.reason : "getparam_3d";
        close(fd);
        return fail(reason);
    }
    if (probe_prime(fd) < 0) {
        close(fd);
        return fail("prime");
    }
    if (probe_sync_file(fd) < 0) {
        close(fd);
        return fail("sync_file");
    }
    close(fd);
    return 0;
}
