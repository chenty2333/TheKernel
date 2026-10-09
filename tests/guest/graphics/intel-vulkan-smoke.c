/*
 * Load Mesa's Intel Vulkan ICD through the guest Vulkan loader and confirm
 * that a no-Intel-GPU guest reports no physical device, not a fake renderer.
 */
#include <stdio.h>
#include <vulkan/vulkan.h>

int main(void)
{
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "TheKernel Intel Vulkan smoke",
        .applicationVersion = 1,
        .pEngineName = "TheKernel guest smoke",
        .engineVersion = 1,
        .apiVersion = VK_API_VERSION_1_0,
    };
    VkInstanceCreateInfo create = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    VkInstance instance = VK_NULL_HANDLE;
    VkResult result = vkCreateInstance(&create, NULL, &instance);
    if (result == VK_ERROR_INCOMPATIBLE_DRIVER) {
        puts("THEKERNEL_N305_ANV_NO_DEVICE_OK result=VK_ERROR_INCOMPATIBLE_DRIVER");
        return 0;
    }
    if (result != VK_SUCCESS) {
        fprintf(stderr, "vkCreateInstance unexpected VkResult=%d\n", result);
        return 1;
    }

    uint32_t count = 0;
    result = vkEnumeratePhysicalDevices(instance, &count, NULL);
    if (result != VK_SUCCESS || count != 0) {
        fprintf(stderr, "expected no Intel GPU, enumerate=%d count=%u\n",
                result, count);
        vkDestroyInstance(instance, NULL);
        return 1;
    }
    vkDestroyInstance(instance, NULL);
    puts("THEKERNEL_N305_ANV_NO_DEVICE_OK result=VK_SUCCESS physical_devices=0");
    return 0;
}
