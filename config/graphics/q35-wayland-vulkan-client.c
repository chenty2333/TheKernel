/*
 * Minimal deterministic Venus workload.  It presents one opaque blue Wayland
 * swapchain image and prints its marker only once presentation has completed.
 * This is intentionally a WSI/conformance probe, not a desktop toolkit.
 */
#define VK_USE_PLATFORM_WAYLAND_KHR

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wayland-client.h>
#include <vulkan/vulkan.h>

#include "q35-venus-xdg-shell-client-protocol.h"

#define MARKER "THEKERNEL_Q35_VENUS_READY"

struct window {
    struct wl_display *display;
    struct wl_compositor *compositor;
    struct xdg_wm_base *wm_base;
    struct wl_surface *surface;
    struct xdg_surface *xdg_surface;
    struct xdg_toplevel *toplevel;
    bool configured;
};

static void fail(const char *reason)
{
    fprintf(stderr, "%s state=FAIL reason=%s\n", MARKER, reason);
    exit(EXIT_FAILURE);
}

static void registry_global(void *data, struct wl_registry *registry,
                            uint32_t name, const char *interface, uint32_t version)
{
    struct window *window = data;
    if (strcmp(interface, wl_compositor_interface.name) == 0) {
        window->compositor = wl_registry_bind(registry, name, &wl_compositor_interface,
                                              version < 4 ? version : 4);
    } else if (strcmp(interface, xdg_wm_base_interface.name) == 0) {
        window->wm_base = wl_registry_bind(registry, name, &xdg_wm_base_interface,
                                           version < 1 ? version : 1);
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
    struct window *window = data;
    xdg_surface_ack_configure(surface, serial);
    window->configured = true;
}

static const struct xdg_surface_listener xdg_surface_listener = {
    .configure = xdg_surface_configure,
};

static void toplevel_configure(void *data, struct xdg_toplevel *toplevel,
                               int32_t width, int32_t height, struct wl_array *states)
{
    (void)data;
    (void)toplevel;
    (void)width;
    (void)height;
    (void)states;
}

static void toplevel_close(void *data, struct xdg_toplevel *toplevel)
{
    (void)data;
    (void)toplevel;
}

static void toplevel_configure_bounds(void *data, struct xdg_toplevel *toplevel,
                                      int32_t width, int32_t height)
{
    (void)data;
    (void)toplevel;
    (void)width;
    (void)height;
}

static void toplevel_wm_capabilities(void *data, struct xdg_toplevel *toplevel,
                                     struct wl_array *capabilities)
{
    (void)data;
    (void)toplevel;
    (void)capabilities;
}

static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure,
    .close = toplevel_close,
    .configure_bounds = toplevel_configure_bounds,
    .wm_capabilities = toplevel_wm_capabilities,
};

static void create_window(struct window *window)
{
    struct wl_registry *registry;

    memset(window, 0, sizeof(*window));
    window->display = wl_display_connect(NULL);
    if (window->display == NULL)
        fail("wayland_connect");
    registry = wl_display_get_registry(window->display);
    wl_registry_add_listener(registry, &registry_listener, window);
    if (wl_display_roundtrip(window->display) < 0 || window->compositor == NULL ||
        window->wm_base == NULL)
        fail("wayland_globals");
    xdg_wm_base_add_listener(window->wm_base, &wm_base_listener, window);
    window->surface = wl_compositor_create_surface(window->compositor);
    window->xdg_surface = xdg_wm_base_get_xdg_surface(window->wm_base, window->surface);
    xdg_surface_add_listener(window->xdg_surface, &xdg_surface_listener, window);
    window->toplevel = xdg_surface_get_toplevel(window->xdg_surface);
    xdg_toplevel_add_listener(window->toplevel, &toplevel_listener, window);
    xdg_toplevel_set_title(window->toplevel, "TheKernel Venus WSI");
    wl_surface_commit(window->surface);
    while (!window->configured && wl_display_dispatch(window->display) >= 0)
        ;
    if (!window->configured)
        fail("wayland_configure");
}

static uint32_t choose_queue_family(VkPhysicalDevice physical_device, VkSurfaceKHR surface)
{
    uint32_t count = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(physical_device, &count, NULL);
    VkQueueFamilyProperties families[count];
    vkGetPhysicalDeviceQueueFamilyProperties(physical_device, &count, families);
    for (uint32_t index = 0; index < count; ++index) {
        VkBool32 present = VK_FALSE;
        if ((families[index].queueFlags & VK_QUEUE_GRAPHICS_BIT) != 0 &&
            vkGetPhysicalDeviceSurfaceSupportKHR(physical_device, index, surface, &present) == VK_SUCCESS && present)
            return index;
    }
    return UINT32_MAX;
}

static VkSurfaceFormatKHR choose_surface_format(VkPhysicalDevice physical_device, VkSurfaceKHR surface)
{
    uint32_t count = 0;
    if (vkGetPhysicalDeviceSurfaceFormatsKHR(physical_device, surface, &count, NULL) != VK_SUCCESS || count == 0)
        fail("surface_formats");
    VkSurfaceFormatKHR formats[count];
    if (vkGetPhysicalDeviceSurfaceFormatsKHR(physical_device, surface, &count, formats) != VK_SUCCESS)
        fail("surface_formats");
    for (uint32_t index = 0; index < count; ++index) {
        if (formats[index].format == VK_FORMAT_B8G8R8A8_SRGB ||
            formats[index].format == VK_FORMAT_B8G8R8A8_UNORM)
            return formats[index];
    }
    return formats[0];
}

int main(void)
{
    struct window window;
    VkInstance instance;
    VkSurfaceKHR surface;
    VkPhysicalDevice physical_device = VK_NULL_HANDLE;
    VkDevice device;
    VkQueue queue;
    VkSwapchainKHR swapchain;
    VkImage image;
    VkImageView view;
    VkRenderPass render_pass;
    VkFramebuffer framebuffer;
    VkCommandPool command_pool;
    VkCommandBuffer command_buffer;
    VkSemaphore acquired;
    VkSemaphore rendered;
    VkFence fence;
    uint32_t queue_family = UINT32_MAX;

    create_window(&window);
    const char *instance_extensions[] = {VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WAYLAND_SURFACE_EXTENSION_NAME};
    const VkApplicationInfo application = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "thekernel-venus-wsi",
        .apiVersion = VK_API_VERSION_1_0,
    };
    const VkInstanceCreateInfo instance_info = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &application,
        .enabledExtensionCount = 2,
        .ppEnabledExtensionNames = instance_extensions,
    };
    if (vkCreateInstance(&instance_info, NULL, &instance) != VK_SUCCESS)
        fail("vk_instance");
    const VkWaylandSurfaceCreateInfoKHR surface_info = {
        .sType = VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR,
        .display = window.display,
        .surface = window.surface,
    };
    if (vkCreateWaylandSurfaceKHR(instance, &surface_info, NULL, &surface) != VK_SUCCESS)
        fail("vk_wayland_surface");

    uint32_t physical_count = 0;
    if (vkEnumeratePhysicalDevices(instance, &physical_count, NULL) != VK_SUCCESS || physical_count == 0)
        fail("physical_device");
    VkPhysicalDevice physical_devices[physical_count];
    if (vkEnumeratePhysicalDevices(instance, &physical_count, physical_devices) != VK_SUCCESS)
        fail("physical_device");
    for (uint32_t index = 0; index < physical_count; ++index) {
        const uint32_t candidate = choose_queue_family(physical_devices[index], surface);
        if (candidate != UINT32_MAX) {
            physical_device = physical_devices[index];
            queue_family = candidate;
            break;
        }
    }
    if (physical_device == VK_NULL_HANDLE)
        fail("graphics_present_queue");

    const float priority = 1.0f;
    const VkDeviceQueueCreateInfo queue_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = queue_family,
        .queueCount = 1,
        .pQueuePriorities = &priority,
    };
    const char *device_extensions[] = {VK_KHR_SWAPCHAIN_EXTENSION_NAME};
    const VkDeviceCreateInfo device_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &queue_info,
        .enabledExtensionCount = 1,
        .ppEnabledExtensionNames = device_extensions,
    };
    if (vkCreateDevice(physical_device, &device_info, NULL, &device) != VK_SUCCESS)
        fail("vk_device");
    vkGetDeviceQueue(device, queue_family, 0, &queue);

    VkSurfaceCapabilitiesKHR capabilities;
    if (vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_device, surface, &capabilities) != VK_SUCCESS)
        fail("surface_capabilities");
    const VkSurfaceFormatKHR format = choose_surface_format(physical_device, surface);
    const VkExtent2D extent = capabilities.currentExtent.width == UINT32_MAX
        ? (VkExtent2D){.width = 320, .height = 240} : capabilities.currentExtent;
    uint32_t image_count = capabilities.minImageCount + 1;
    if (capabilities.maxImageCount != 0 && image_count > capabilities.maxImageCount)
        image_count = capabilities.maxImageCount;
    const VkSwapchainCreateInfoKHR swapchain_info = {
        .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
        .surface = surface,
        .minImageCount = image_count,
        .imageFormat = format.format,
        .imageColorSpace = format.colorSpace,
        .imageExtent = extent,
        .imageArrayLayers = 1,
        .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
        .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .preTransform = capabilities.currentTransform,
        .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
        .presentMode = VK_PRESENT_MODE_FIFO_KHR,
        .clipped = VK_TRUE,
    };
    if (vkCreateSwapchainKHR(device, &swapchain_info, NULL, &swapchain) != VK_SUCCESS)
        fail("swapchain");
    uint32_t swapchain_images = 0;
    if (vkGetSwapchainImagesKHR(device, swapchain, &swapchain_images, NULL) != VK_SUCCESS || swapchain_images == 0)
        fail("swapchain_image");
    VkImage images[swapchain_images];
    if (vkGetSwapchainImagesKHR(device, swapchain, &swapchain_images, images) != VK_SUCCESS)
        fail("swapchain_image");
    image = images[0];

    const VkImageViewCreateInfo view_info = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
        .image = image,
        .viewType = VK_IMAGE_VIEW_TYPE_2D,
        .format = format.format,
        .subresourceRange = {
            .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
            .levelCount = 1,
            .layerCount = 1,
        },
    };
    if (vkCreateImageView(device, &view_info, NULL, &view) != VK_SUCCESS)
        fail("image_view");
    const VkAttachmentDescription attachment = {
        .format = format.format,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
        .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        .finalLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
    };
    const VkAttachmentReference color = {.attachment = 0, .layout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    const VkSubpassDescription subpass = {.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS, .colorAttachmentCount = 1, .pColorAttachments = &color};
    const VkRenderPassCreateInfo render_pass_info = {
        .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
        .attachmentCount = 1,
        .pAttachments = &attachment,
        .subpassCount = 1,
        .pSubpasses = &subpass,
    };
    if (vkCreateRenderPass(device, &render_pass_info, NULL, &render_pass) != VK_SUCCESS)
        fail("render_pass");
    const VkFramebufferCreateInfo framebuffer_info = {
        .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
        .renderPass = render_pass,
        .attachmentCount = 1,
        .pAttachments = &view,
        .width = extent.width,
        .height = extent.height,
        .layers = 1,
    };
    if (vkCreateFramebuffer(device, &framebuffer_info, NULL, &framebuffer) != VK_SUCCESS)
        fail("framebuffer");
    const VkCommandPoolCreateInfo pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = queue_family,
    };
    if (vkCreateCommandPool(device, &pool_info, NULL, &command_pool) != VK_SUCCESS)
        fail("command_pool");
    const VkCommandBufferAllocateInfo command_buffer_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = command_pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    if (vkAllocateCommandBuffers(device, &command_buffer_info, &command_buffer) != VK_SUCCESS)
        fail("command_buffer");
    const VkCommandBufferBeginInfo begin_info = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    if (vkBeginCommandBuffer(command_buffer, &begin_info) != VK_SUCCESS)
        fail("command_begin");
    const VkClearValue clear = {.color = {.float32 = {0.08f, 0.26f, 0.75f, 1.0f}}};
    const VkRenderPassBeginInfo pass_begin = {
        .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
        .renderPass = render_pass,
        .framebuffer = framebuffer,
        .renderArea = {.extent = extent},
        .clearValueCount = 1,
        .pClearValues = &clear,
    };
    vkCmdBeginRenderPass(command_buffer, &pass_begin, VK_SUBPASS_CONTENTS_INLINE);
    vkCmdEndRenderPass(command_buffer);
    if (vkEndCommandBuffer(command_buffer) != VK_SUCCESS)
        fail("command_end");
    const VkSemaphoreCreateInfo semaphore_info = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
    const VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    if (vkCreateSemaphore(device, &semaphore_info, NULL, &acquired) != VK_SUCCESS ||
        vkCreateSemaphore(device, &semaphore_info, NULL, &rendered) != VK_SUCCESS ||
        vkCreateFence(device, &fence_info, NULL, &fence) != VK_SUCCESS)
        fail("sync_objects");
    uint32_t image_index = 0;
    if (vkAcquireNextImageKHR(device, swapchain, UINT64_MAX, acquired, VK_NULL_HANDLE, &image_index) != VK_SUCCESS)
        fail("acquire");
    const VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    const VkSubmitInfo submit = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .waitSemaphoreCount = 1,
        .pWaitSemaphores = &acquired,
        .pWaitDstStageMask = &wait_stage,
        .commandBufferCount = 1,
        .pCommandBuffers = &command_buffer,
        .signalSemaphoreCount = 1,
        .pSignalSemaphores = &rendered,
    };
    if (vkQueueSubmit(queue, 1, &submit, fence) != VK_SUCCESS ||
        vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX) != VK_SUCCESS)
        fail("submit");
    const VkPresentInfoKHR present = {
        .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
        .waitSemaphoreCount = 1,
        .pWaitSemaphores = &rendered,
        .swapchainCount = 1,
        .pSwapchains = &swapchain,
        .pImageIndices = &image_index,
    };
    if (vkQueuePresentKHR(queue, &present) != VK_SUCCESS || vkQueueWaitIdle(queue) != VK_SUCCESS)
        fail("present");
    printf("%s\n", MARKER);
    fflush(stdout);
    return EXIT_SUCCESS;
}
