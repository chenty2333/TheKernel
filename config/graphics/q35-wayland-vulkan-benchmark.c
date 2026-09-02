/*
 * A deliberately small Venus/Wayland WSI benchmark.  A wl_surface frame
 * callback gates each new Vulkan present, so the numbers describe completed
 * compositor frames rather than an unbounded queue of vkQueuePresentKHR calls.
 */
#define _POSIX_C_SOURCE 200809L
#define VK_USE_PLATFORM_WAYLAND_KHR

#include <ctype.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>
#include <vulkan/vulkan.h>

#include "q35-venus-xdg-shell-client-protocol.h"

#ifndef THEKERNEL_WIDTH
#define THEKERNEL_WIDTH 3840U
#endif
#ifndef THEKERNEL_HEIGHT
#define THEKERNEL_HEIGHT 2160U
#endif

enum { METRIC_FIRST = 60U, METRIC_END = 660U };

struct app {
    struct wl_display *display;
    struct wl_compositor *compositor;
    struct wl_seat *seat;
    struct wl_pointer *pointer;
    struct wl_keyboard *keyboard;
    struct xdg_wm_base *wm;
    struct wl_surface *surface;
    struct xdg_surface *xdg_surface;
    struct xdg_toplevel *toplevel;
    int configured;
    int keyboard_focused;
    int done;
    uint32_t frame_index;
    uint32_t abort_after;
    uint64_t previous_frame_ns;
    uint64_t input_ns;
    uint64_t frame_input_ns;
    uint32_t next_input_sequence;
    uint32_t frame_input_sequence;
    int input_armed;
    int input_pending;
    int frame_has_input;
    int input_state;
    int completed;

    VkInstance instance;
    VkSurfaceKHR vk_surface;
    VkPhysicalDevice physical_device;
    VkDevice device;
    VkQueue queue;
    uint32_t queue_family;
    VkSwapchainKHR swapchain;
    VkExtent2D extent;
    VkFormat format;
    uint32_t image_count;
    VkImage *images;
    VkImageView *views;
    VkFramebuffer *framebuffers;
    VkRenderPass render_pass;
    VkCommandPool command_pool;
    VkCommandBuffer *command_buffers;
    VkSemaphore acquired;
    VkSemaphore rendered;
    VkFence submit_fence;
};

static uint64_t monotonic_ns(void)
{
    struct timespec ts;
    (void)clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * UINT64_C(1000000000) + (uint64_t)ts.tv_nsec;
}

static void fail(const char *reason)
{
    fprintf(stderr, "THEKERNEL_GRAPHICS_BENCHMARK state=FAIL reason=%s\n", reason);
}

static int vk_ok(VkResult result, const char *reason)
{
    if (result == VK_SUCCESS)
        return 1;
    fail(reason);
    return 0;
}

static void metric_frame(struct app *app)
{
    const uint64_t now = monotonic_ns();
    if (app->previous_frame_ns != 0U && app->frame_index >= METRIC_FIRST &&
        app->frame_index < METRIC_END) {
        printf("THEKERNEL_GRAPHICS_METRIC {\"kind\":\"frame\",\"index\":%u,\"ns\":%llu}\n",
               app->frame_index,
               (unsigned long long)(now - app->previous_frame_ns));
    }
    app->previous_frame_ns = now;
    ++app->frame_index;
    fflush(stdout);
}

static void request_input_repaint(struct app *app)
{
    if (app->input_armed && !app->input_pending && !app->frame_has_input) {
        app->input_ns = monotonic_ns();
        app->input_pending = 1;
        app->input_state = !app->input_state;
    }
}

static void pointer_enter(void *data, struct wl_pointer *pointer, uint32_t serial,
                          struct wl_surface *surface, wl_fixed_t sx, wl_fixed_t sy)
{
    (void)data; (void)pointer; (void)serial; (void)surface; (void)sx; (void)sy;
}
static void pointer_leave(void *data, struct wl_pointer *pointer, uint32_t serial,
                          struct wl_surface *surface)
{
    (void)data; (void)pointer; (void)serial; (void)surface;
}
static void pointer_motion(void *data, struct wl_pointer *pointer, uint32_t time,
                           wl_fixed_t sx, wl_fixed_t sy)
{
    (void)pointer; (void)time; (void)sx; (void)sy;
    request_input_repaint(data);
}
static void pointer_button(void *data, struct wl_pointer *pointer, uint32_t serial,
                           uint32_t time, uint32_t button, uint32_t state)
{
    (void)pointer; (void)serial; (void)time; (void)button;
    if (state != 0U)
        request_input_repaint(data);
}
static void pointer_axis(void *data, struct wl_pointer *pointer, uint32_t time,
                         uint32_t axis, wl_fixed_t value)
{
    (void)pointer; (void)time; (void)axis; (void)value;
    request_input_repaint(data);
}
static const struct wl_pointer_listener pointer_listener = {
    .enter = pointer_enter, .leave = pointer_leave, .motion = pointer_motion,
    .button = pointer_button, .axis = pointer_axis,
};

static void keyboard_keymap(void *data, struct wl_keyboard *keyboard, uint32_t format,
                            int fd, uint32_t size)
{
    (void)data; (void)keyboard; (void)format; (void)size;
    if (fd >= 0)
        (void)close(fd);
}
static void keyboard_enter(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                           struct wl_surface *surface, struct wl_array *keys)
{
    struct app *app = data;
    (void)keyboard; (void)serial; (void)surface; (void)keys;
    app->keyboard_focused = 1;
}
static void keyboard_leave(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                           struct wl_surface *surface)
{
    struct app *app = data;
    (void)keyboard; (void)serial; (void)surface;
    app->keyboard_focused = 0;
}
static void keyboard_key(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                         uint32_t time, uint32_t key, uint32_t state)
{
    (void)keyboard; (void)serial; (void)time; (void)key;
    if (state != 0U)
        request_input_repaint(data);
}
static void keyboard_modifiers(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                               uint32_t depressed, uint32_t latched, uint32_t locked,
                               uint32_t group)
{
    (void)data; (void)keyboard; (void)serial; (void)depressed; (void)latched;
    (void)locked; (void)group;
}
static void keyboard_repeat_info(void *data, struct wl_keyboard *keyboard, int32_t rate,
                                 int32_t delay)
{
    (void)data; (void)keyboard; (void)rate; (void)delay;
}
static const struct wl_keyboard_listener keyboard_listener = {
    .keymap = keyboard_keymap, .enter = keyboard_enter, .leave = keyboard_leave,
    .key = keyboard_key, .modifiers = keyboard_modifiers, .repeat_info = keyboard_repeat_info,
};

static void seat_capabilities(void *data, struct wl_seat *seat, uint32_t capabilities)
{
    struct app *app = data;
    if ((capabilities & WL_SEAT_CAPABILITY_POINTER) != 0U && app->pointer == NULL) {
        app->pointer = wl_seat_get_pointer(seat);
        if (app->pointer != NULL)
            wl_pointer_add_listener(app->pointer, &pointer_listener, app);
    }
    if ((capabilities & WL_SEAT_CAPABILITY_KEYBOARD) != 0U && app->keyboard == NULL) {
        app->keyboard = wl_seat_get_keyboard(seat);
        if (app->keyboard != NULL)
            wl_keyboard_add_listener(app->keyboard, &keyboard_listener, app);
    }
}
static void seat_name(void *data, struct wl_seat *seat, const char *name)
{
    (void)data; (void)seat; (void)name;
}
static const struct wl_seat_listener seat_listener = {
    .capabilities = seat_capabilities, .name = seat_name,
};

static void wm_ping(void *data, struct xdg_wm_base *wm, uint32_t serial)
{
    (void)data;
    xdg_wm_base_pong(wm, serial);
}
static const struct xdg_wm_base_listener wm_listener = { .ping = wm_ping };

static void xdg_configure(void *data, struct xdg_surface *surface, uint32_t serial)
{
    struct app *app = data;
    xdg_surface_ack_configure(surface, serial);
    app->configured = 1;
}
static const struct xdg_surface_listener xdg_listener = { .configure = xdg_configure };

static void toplevel_configure(void *data, struct xdg_toplevel *toplevel, int32_t width,
                               int32_t height, struct wl_array *states)
{
    (void)data; (void)toplevel; (void)width; (void)height; (void)states;
}
static void toplevel_close(void *data, struct xdg_toplevel *toplevel)
{
    struct app *app = data;
    (void)toplevel;
    app->done = 1;
}
static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure, .close = toplevel_close,
};

static void registry_global(void *data, struct wl_registry *registry, uint32_t name,
                            const char *interface, uint32_t version)
{
    struct app *app = data;
    if (strcmp(interface, wl_compositor_interface.name) == 0) {
        app->compositor = wl_registry_bind(registry, name, &wl_compositor_interface,
                                           version < 4U ? version : 4U);
    } else if (strcmp(interface, xdg_wm_base_interface.name) == 0) {
        app->wm = wl_registry_bind(registry, name, &xdg_wm_base_interface, 1U);
        if (app->wm != NULL)
            xdg_wm_base_add_listener(app->wm, &wm_listener, app);
    } else if (strcmp(interface, wl_seat_interface.name) == 0) {
        app->seat = wl_registry_bind(registry, name, &wl_seat_interface,
                                     version < 5U ? version : 5U);
        if (app->seat != NULL)
            wl_seat_add_listener(app->seat, &seat_listener, app);
    }
}
static void registry_global_remove(void *data, struct wl_registry *registry, uint32_t name)
{
    (void)data; (void)registry; (void)name;
}
static const struct wl_registry_listener registry_listener = {
    .global = registry_global, .global_remove = registry_global_remove,
};

static int device_is_virtio(VkPhysicalDevice device)
{
    VkPhysicalDeviceProperties properties;
    char lower[VK_MAX_PHYSICAL_DEVICE_NAME_SIZE];
    size_t i;
    vkGetPhysicalDeviceProperties(device, &properties);
    for (i = 0U; i + 1U < sizeof(lower) && properties.deviceName[i] != '\0'; ++i)
        lower[i] = (char)tolower((unsigned char)properties.deviceName[i]);
    lower[i] = '\0';
    return strstr(lower, "virtio") != NULL;
}

static uint32_t choose_queue_family(VkPhysicalDevice device, VkSurfaceKHR surface)
{
    uint32_t count = 0U;
    uint32_t index;
    VkQueueFamilyProperties *families;
    vkGetPhysicalDeviceQueueFamilyProperties(device, &count, NULL);
    if (count == 0U)
        return UINT32_MAX;
    families = calloc(count, sizeof(*families));
    if (families == NULL)
        return UINT32_MAX;
    vkGetPhysicalDeviceQueueFamilyProperties(device, &count, families);
    for (index = 0U; index < count; ++index) {
        VkBool32 present = VK_FALSE;
        if ((families[index].queueFlags & VK_QUEUE_GRAPHICS_BIT) != 0U &&
            vkGetPhysicalDeviceSurfaceSupportKHR(device, index, surface, &present) == VK_SUCCESS &&
            present == VK_TRUE) {
            free(families);
            return index;
        }
    }
    free(families);
    return UINT32_MAX;
}

static int create_window(struct app *app)
{
    struct wl_registry *registry;
    app->display = wl_display_connect(NULL);
    if (app->display == NULL)
        return 0;
    registry = wl_display_get_registry(app->display);
    wl_registry_add_listener(registry, &registry_listener, app);
    if (wl_display_roundtrip(app->display) < 0 || app->compositor == NULL || app->wm == NULL ||
        app->seat == NULL)
        return 0;
    app->surface = wl_compositor_create_surface(app->compositor);
    app->xdg_surface = xdg_wm_base_get_xdg_surface(app->wm, app->surface);
    if (app->surface == NULL || app->xdg_surface == NULL)
        return 0;
    xdg_surface_add_listener(app->xdg_surface, &xdg_listener, app);
    app->toplevel = xdg_surface_get_toplevel(app->xdg_surface);
    if (app->toplevel == NULL)
        return 0;
    xdg_toplevel_add_listener(app->toplevel, &toplevel_listener, app);
    xdg_toplevel_set_title(app->toplevel, "TheKernel Venus fullscreen benchmark");
    xdg_toplevel_set_fullscreen(app->toplevel, NULL);
    wl_surface_commit(app->surface);
    while (!app->configured && wl_display_dispatch(app->display) >= 0)
        ;
    if (!app->configured)
        return 0;
    /* A second roundtrip receives seat capabilities and the initial enter event. */
    return wl_display_roundtrip(app->display) >= 0 && app->pointer != NULL &&
           app->keyboard != NULL && app->keyboard_focused;
}

static int choose_surface_format(struct app *app, VkSurfaceFormatKHR *selected)
{
    VkSurfaceFormatKHR *formats;
    uint32_t count = 0U;
    uint32_t index;
    if (!vk_ok(vkGetPhysicalDeviceSurfaceFormatsKHR(app->physical_device, app->vk_surface,
                                                     &count, NULL), "surface_formats") || count == 0U)
        return 0;
    formats = calloc(count, sizeof(*formats));
    if (formats == NULL || !vk_ok(vkGetPhysicalDeviceSurfaceFormatsKHR(app->physical_device,
                   app->vk_surface, &count, formats), "surface_formats")) {
        free(formats);
        return 0;
    }
    *selected = formats[0];
    for (index = 0U; index < count; ++index) {
        if (formats[index].format == VK_FORMAT_B8G8R8A8_SRGB ||
            formats[index].format == VK_FORMAT_B8G8R8A8_UNORM) {
            *selected = formats[index];
            break;
        }
    }
    free(formats);
    return 1;
}

static int create_vulkan(struct app *app)
{
    const char *instance_extensions[] = {
        VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WAYLAND_SURFACE_EXTENSION_NAME,
    };
    const VkApplicationInfo application = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "thekernel-venus-fullscreen-benchmark",
        .apiVersion = VK_API_VERSION_1_0,
    };
    const VkInstanceCreateInfo instance_info = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &application,
        .enabledExtensionCount = 2U, .ppEnabledExtensionNames = instance_extensions,
    };
    const VkWaylandSurfaceCreateInfoKHR wayland_surface = {
        .sType = VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR,
        .display = app->display, .surface = app->surface,
    };
    VkPhysicalDevice *devices;
    uint32_t device_count = 0U;
    uint32_t index;
    const float priority = 1.0f;
    const char *device_extensions[] = { VK_KHR_SWAPCHAIN_EXTENSION_NAME };
    VkDeviceQueueCreateInfo queue_info;
    VkDeviceCreateInfo device_info;

    if (!vk_ok(vkCreateInstance(&instance_info, NULL, &app->instance), "vk_instance") ||
        !vk_ok(vkCreateWaylandSurfaceKHR(app->instance, &wayland_surface, NULL,
                                         &app->vk_surface), "vk_wayland_surface") ||
        !vk_ok(vkEnumeratePhysicalDevices(app->instance, &device_count, NULL), "physical_devices") ||
        device_count == 0U)
        return 0;
    devices = calloc(device_count, sizeof(*devices));
    if (devices == NULL || !vk_ok(vkEnumeratePhysicalDevices(app->instance, &device_count, devices),
                                  "physical_devices")) {
        free(devices);
        return 0;
    }
    for (index = 0U; index < device_count; ++index) {
        const uint32_t queue_family = choose_queue_family(devices[index], app->vk_surface);
        if (queue_family != UINT32_MAX && device_is_virtio(devices[index])) {
            app->physical_device = devices[index];
            app->queue_family = queue_family;
            break;
        }
    }
    free(devices);
    if (app->physical_device == VK_NULL_HANDLE) {
        fail("virtio_graphics_present_queue");
        return 0;
    }
    queue_info = (VkDeviceQueueCreateInfo){
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = app->queue_family,
        .queueCount = 1U, .pQueuePriorities = &priority,
    };
    device_info = (VkDeviceCreateInfo){
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1U,
        .pQueueCreateInfos = &queue_info, .enabledExtensionCount = 1U,
        .ppEnabledExtensionNames = device_extensions,
    };
    if (!vk_ok(vkCreateDevice(app->physical_device, &device_info, NULL, &app->device), "vk_device"))
        return 0;
    vkGetDeviceQueue(app->device, app->queue_family, 0U, &app->queue);
    return 1;
}

static int create_swapchain(struct app *app)
{
    VkSurfaceCapabilitiesKHR capabilities;
    VkSurfaceFormatKHR surface_format;
    VkSwapchainCreateInfoKHR info;
    uint32_t desired_count;
    uint32_t index;

    if (!vk_ok(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(app->physical_device, app->vk_surface,
                                                          &capabilities), "surface_capabilities") ||
        !choose_surface_format(app, &surface_format))
        return 0;
    app->extent = capabilities.currentExtent;
    if (app->extent.width == UINT32_MAX) {
        app->extent.width = THEKERNEL_WIDTH;
        app->extent.height = THEKERNEL_HEIGHT;
        if (app->extent.width < capabilities.minImageExtent.width)
            app->extent.width = capabilities.minImageExtent.width;
        if (app->extent.height < capabilities.minImageExtent.height)
            app->extent.height = capabilities.minImageExtent.height;
        if (app->extent.width > capabilities.maxImageExtent.width)
            app->extent.width = capabilities.maxImageExtent.width;
        if (app->extent.height > capabilities.maxImageExtent.height)
            app->extent.height = capabilities.maxImageExtent.height;
    }
    desired_count = capabilities.minImageCount + 1U;
    if (capabilities.maxImageCount != 0U && desired_count > capabilities.maxImageCount)
        desired_count = capabilities.maxImageCount;
    info = (VkSwapchainCreateInfoKHR){
        .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR, .surface = app->vk_surface,
        .minImageCount = desired_count, .imageFormat = surface_format.format,
        .imageColorSpace = surface_format.colorSpace, .imageExtent = app->extent,
        .imageArrayLayers = 1U, .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
        .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE, .preTransform = capabilities.currentTransform,
        .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR, .presentMode = VK_PRESENT_MODE_FIFO_KHR,
        .clipped = VK_TRUE,
    };
    if (!vk_ok(vkCreateSwapchainKHR(app->device, &info, NULL, &app->swapchain), "swapchain") ||
        !vk_ok(vkGetSwapchainImagesKHR(app->device, app->swapchain, &app->image_count, NULL),
               "swapchain_images") || app->image_count == 0U)
        return 0;
    app->images = calloc(app->image_count, sizeof(*app->images));
    app->views = calloc(app->image_count, sizeof(*app->views));
    app->framebuffers = calloc(app->image_count, sizeof(*app->framebuffers));
    app->command_buffers = calloc(app->image_count, sizeof(*app->command_buffers));
    if (app->images == NULL || app->views == NULL || app->framebuffers == NULL ||
        app->command_buffers == NULL ||
        !vk_ok(vkGetSwapchainImagesKHR(app->device, app->swapchain, &app->image_count, app->images),
               "swapchain_images"))
        return 0;
    app->format = surface_format.format;
    for (index = 0U; index < app->image_count; ++index) {
        const VkImageViewCreateInfo view_info = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = app->images[index],
            .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = app->format,
            .subresourceRange = { .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT, .levelCount = 1U,
                                  .layerCount = 1U },
        };
        if (!vk_ok(vkCreateImageView(app->device, &view_info, NULL, &app->views[index]),
                   "image_view"))
            return 0;
    }
    return 1;
}

static int create_rendering(struct app *app)
{
    const VkAttachmentDescription attachment = {
        .format = app->format, .samples = VK_SAMPLE_COUNT_1_BIT,
        .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED, .finalLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
    };
    const VkAttachmentReference color = {
        .attachment = 0U, .layout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
    };
    const VkSubpassDescription subpass = {
        .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS, .colorAttachmentCount = 1U,
        .pColorAttachments = &color,
    };
    const VkRenderPassCreateInfo render_pass_info = {
        .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO, .attachmentCount = 1U,
        .pAttachments = &attachment, .subpassCount = 1U, .pSubpasses = &subpass,
    };
    VkCommandPoolCreateInfo pool_info;
    VkCommandBufferAllocateInfo buffer_info;
    VkSemaphoreCreateInfo semaphore_info;
    VkFenceCreateInfo fence_info;
    uint32_t index;
    if (!vk_ok(vkCreateRenderPass(app->device, &render_pass_info, NULL, &app->render_pass),
               "render_pass"))
        return 0;
    for (index = 0U; index < app->image_count; ++index) {
        const VkFramebufferCreateInfo framebuffer_info = {
            .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO, .renderPass = app->render_pass,
            .attachmentCount = 1U, .pAttachments = &app->views[index],
            .width = app->extent.width, .height = app->extent.height, .layers = 1U,
        };
        if (!vk_ok(vkCreateFramebuffer(app->device, &framebuffer_info, NULL,
                                       &app->framebuffers[index]), "framebuffer"))
            return 0;
    }
    pool_info = (VkCommandPoolCreateInfo){
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
        .queueFamilyIndex = app->queue_family,
    };
    buffer_info = (VkCommandBufferAllocateInfo){
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = VK_NULL_HANDLE,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = app->image_count,
    };
    if (!vk_ok(vkCreateCommandPool(app->device, &pool_info, NULL, &app->command_pool), "command_pool"))
        return 0;
    buffer_info.commandPool = app->command_pool;
    if (!vk_ok(vkAllocateCommandBuffers(app->device, &buffer_info, app->command_buffers),
               "command_buffers"))
        return 0;
    semaphore_info = (VkSemaphoreCreateInfo){ .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
    fence_info = (VkFenceCreateInfo){ .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
                                       .flags = VK_FENCE_CREATE_SIGNALED_BIT };
    return vk_ok(vkCreateSemaphore(app->device, &semaphore_info, NULL, &app->acquired),
                 "acquire_semaphore") &&
           vk_ok(vkCreateSemaphore(app->device, &semaphore_info, NULL, &app->rendered),
                 "render_semaphore") &&
           vk_ok(vkCreateFence(app->device, &fence_info, NULL, &app->submit_fence), "submit_fence");
}

static void frame_done(void *data, struct wl_callback *callback, uint32_t time);
static const struct wl_callback_listener frame_listener = { .done = frame_done };

static int render_frame(struct app *app)
{
    uint32_t image_index;
    VkResult result;
    VkCommandBuffer command_buffer;
    VkCommandBufferBeginInfo begin_info;
    VkClearValue clear;
    VkRenderPassBeginInfo pass_begin;
    VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    VkSubmitInfo submit;
    VkPresentInfoKHR present;
    struct wl_callback *callback;
    const float phase = (float)(app->frame_index % 360U) / 359.0f;

    if (app->input_pending) {
        app->frame_has_input = 1;
        app->frame_input_sequence = app->next_input_sequence;
        app->frame_input_ns = app->input_ns;
        app->input_pending = 0;
    }
    if (!vk_ok(vkWaitForFences(app->device, 1U, &app->submit_fence, VK_TRUE, UINT64_MAX),
               "submit_wait") ||
        !vk_ok(vkResetFences(app->device, 1U, &app->submit_fence), "submit_reset"))
        return 0;
    result = vkAcquireNextImageKHR(app->device, app->swapchain, UINT64_MAX, app->acquired,
                                   VK_NULL_HANDLE, &image_index);
    if (result == VK_ERROR_OUT_OF_DATE_KHR || result == VK_SUBOPTIMAL_KHR) {
        fail("swapchain_recreate_required");
        return 0;
    }
    if (!vk_ok(result, "acquire"))
        return 0;
    command_buffer = app->command_buffers[image_index];
    if (!vk_ok(vkResetCommandBuffer(command_buffer, 0U), "command_reset"))
        return 0;
    begin_info = (VkCommandBufferBeginInfo){ .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
    if (!vk_ok(vkBeginCommandBuffer(command_buffer, &begin_info), "command_begin"))
        return 0;
    clear = (VkClearValue){ .color = { .float32 = {
        app->input_state ? 0.9f : 0.1f,
        0.25f + 0.5f * (1.0f - phase),
        app->input_state ? 0.15f : 0.85f,
        1.0f,
    } } };
    pass_begin = (VkRenderPassBeginInfo){
        .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO, .renderPass = app->render_pass,
        .framebuffer = app->framebuffers[image_index], .renderArea = { .extent = app->extent },
        .clearValueCount = 1U, .pClearValues = &clear,
    };
    vkCmdBeginRenderPass(command_buffer, &pass_begin, VK_SUBPASS_CONTENTS_INLINE);
    vkCmdEndRenderPass(command_buffer);
    if (!vk_ok(vkEndCommandBuffer(command_buffer), "command_end"))
        return 0;
    submit = (VkSubmitInfo){
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .waitSemaphoreCount = 1U,
        .pWaitSemaphores = &app->acquired, .pWaitDstStageMask = &wait_stage,
        .commandBufferCount = 1U, .pCommandBuffers = &command_buffer,
        .signalSemaphoreCount = 1U, .pSignalSemaphores = &app->rendered,
    };
    if (!vk_ok(vkQueueSubmit(app->queue, 1U, &submit, app->submit_fence), "submit"))
        return 0;
    callback = wl_surface_frame(app->surface);
    if (callback == NULL) {
        fail("frame_callback");
        return 0;
    }
    wl_surface_damage_buffer(app->surface, 0, 0, (int32_t)app->extent.width,
                             (int32_t)app->extent.height);
    present = (VkPresentInfoKHR){
        .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR, .waitSemaphoreCount = 1U,
        .pWaitSemaphores = &app->rendered, .swapchainCount = 1U, .pSwapchains = &app->swapchain,
        .pImageIndices = &image_index,
    };
    result = vkQueuePresentKHR(app->queue, &present);
    if (result == VK_ERROR_OUT_OF_DATE_KHR || result == VK_SUBOPTIMAL_KHR) {
        wl_callback_destroy(callback);
        fail("swapchain_recreate_required");
        return 0;
    }
    if (!vk_ok(result, "present")) {
        wl_callback_destroy(callback);
        return 0;
    }
    wl_callback_add_listener(callback, &frame_listener, app);
    return 1;
}

static void frame_done(void *data, struct wl_callback *callback, uint32_t time)
{
    struct app *app = data;
    (void)time;
    wl_callback_destroy(callback);
    metric_frame(app);
    if (app->frame_has_input) {
        printf("THEKERNEL_GRAPHICS_METRIC {\"kind\":\"input_to_repaint\",\"ns\":%llu}\n",
               (unsigned long long)(monotonic_ns() - app->frame_input_ns));
        printf("THEKERNEL_GRAPHICS_INPUT_VISIBLE_%03u\n", app->frame_input_sequence);
        fflush(stdout);
        ++app->next_input_sequence;
        app->frame_has_input = 0;
    }
    if (app->abort_after != 0U && app->frame_index >= app->abort_after)
        abort();
    if (app->frame_index >= METRIC_END) {
        app->completed = 1;
        app->done = 1;
        return;
    }
    if (!render_frame(app))
        app->done = 1;
}

static int begin_frame(struct app *app)
{
    return render_frame(app);
}

static void destroy_app(struct app *app)
{
    uint32_t index;
    if (app->device != VK_NULL_HANDLE)
        (void)vkDeviceWaitIdle(app->device);
    if (app->device != VK_NULL_HANDLE) {
        if (app->submit_fence != VK_NULL_HANDLE) vkDestroyFence(app->device, app->submit_fence, NULL);
        if (app->rendered != VK_NULL_HANDLE) vkDestroySemaphore(app->device, app->rendered, NULL);
        if (app->acquired != VK_NULL_HANDLE) vkDestroySemaphore(app->device, app->acquired, NULL);
        if (app->command_pool != VK_NULL_HANDLE) vkDestroyCommandPool(app->device, app->command_pool, NULL);
        if (app->framebuffers != NULL) for (index = 0U; index < app->image_count; ++index)
            if (app->framebuffers[index] != VK_NULL_HANDLE) vkDestroyFramebuffer(app->device, app->framebuffers[index], NULL);
        if (app->render_pass != VK_NULL_HANDLE) vkDestroyRenderPass(app->device, app->render_pass, NULL);
        if (app->views != NULL) for (index = 0U; index < app->image_count; ++index)
            if (app->views[index] != VK_NULL_HANDLE) vkDestroyImageView(app->device, app->views[index], NULL);
        if (app->swapchain != VK_NULL_HANDLE) vkDestroySwapchainKHR(app->device, app->swapchain, NULL);
        vkDestroyDevice(app->device, NULL);
    }
    free(app->command_buffers); free(app->framebuffers); free(app->views); free(app->images);
    if (app->vk_surface != VK_NULL_HANDLE) vkDestroySurfaceKHR(app->instance, app->vk_surface, NULL);
    if (app->instance != VK_NULL_HANDLE) vkDestroyInstance(app->instance, NULL);
    if (app->keyboard != NULL) wl_keyboard_destroy(app->keyboard);
    if (app->pointer != NULL) wl_pointer_destroy(app->pointer);
    if (app->toplevel != NULL) xdg_toplevel_destroy(app->toplevel);
    if (app->xdg_surface != NULL) xdg_surface_destroy(app->xdg_surface);
    if (app->surface != NULL) wl_surface_destroy(app->surface);
    if (app->wm != NULL) xdg_wm_base_destroy(app->wm);
    if (app->seat != NULL) wl_seat_destroy(app->seat);
    if (app->compositor != NULL) wl_compositor_destroy(app->compositor);
    if (app->display != NULL) wl_display_disconnect(app->display);
}

int main(void)
{
    struct app app;
    const char *abort_after;
    int exit_code = EXIT_FAILURE;
    memset(&app, 0, sizeof(app));
    abort_after = getenv("THEKERNEL_GRAPHICS_ABORT_AFTER");
    if (abort_after != NULL)
        app.abort_after = (uint32_t)strtoul(abort_after, NULL, 10);
    if (!create_window(&app)) {
        fail("input_focus_roundtrip");
        goto out;
    }
    if (!create_vulkan(&app) || !create_swapchain(&app) || !create_rendering(&app))
        goto out;
    puts("THEKERNEL_GRAPHICS_BENCHMARK_READY");
    fflush(stdout);
    app.input_armed = 1;
    if (!begin_frame(&app))
        goto out;
    while (!app.done && wl_display_dispatch(app.display) >= 0)
        ;
    exit_code = app.completed ? EXIT_SUCCESS : EXIT_FAILURE;
out:
    destroy_app(&app);
    return exit_code;
}
