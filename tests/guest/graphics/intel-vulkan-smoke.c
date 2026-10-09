/*
 * Prove both guest-loader ICD selection and the ANV ICD's own no-device result.
 * This is a no-Intel-GPU loader check, not evidence of N305 GPU readiness.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <vulkan/vulkan.h>
#include <vulkan/vk_icd.h>

static int direct_anv_no_device(const VkInstanceCreateInfo *create)
{
    const char *path = "/usr/lib/libvulkan_intel.so";
    void *module = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    if (!module) {
        fprintf(stderr, "ANV direct ICD dlopen failed: %s\n", dlerror());
        return 1;
    }

    dlerror();
    PFN_vk_icdGetInstanceProcAddr get_proc_addr =
        (PFN_vk_icdGetInstanceProcAddr)dlsym(module, "vk_icdGetInstanceProcAddr");
    const char *error = dlerror();
    if (error || !get_proc_addr) {
        fprintf(stderr, "ANV direct ICD entrypoint missing: %s\n",
                error ? error : "missing symbol");
        dlclose(module);
        return 1;
    }

    PFN_vkCreateInstance create_instance =
        (PFN_vkCreateInstance)get_proc_addr(VK_NULL_HANDLE, "vkCreateInstance");
    if (!create_instance) {
        fprintf(stderr, "ANV direct ICD vkCreateInstance missing\n");
        dlclose(module);
        return 1;
    }

    VkInstance instance = VK_NULL_HANDLE;
    VkResult result = create_instance(create, NULL, &instance);
    if (result != VK_SUCCESS || instance == VK_NULL_HANDLE) {
        fprintf(stderr, "ANV direct ICD vkCreateInstance result=%d\n", result);
        dlclose(module);
        return 1;
    }

    PFN_vkEnumeratePhysicalDevices enumerate =
        (PFN_vkEnumeratePhysicalDevices)get_proc_addr(instance, "vkEnumeratePhysicalDevices");
    PFN_vkDestroyInstance destroy_instance =
        (PFN_vkDestroyInstance)get_proc_addr(instance, "vkDestroyInstance");
    if (!enumerate || !destroy_instance) {
        fprintf(stderr, "ANV direct ICD enumeration/destroy entrypoint missing\n");
        if (destroy_instance)
            destroy_instance(instance, NULL);
        dlclose(module);
        return 1;
    }

    uint32_t count = 0;
    result = enumerate(instance, &count, NULL);
    if (result != VK_SUCCESS || count != 0) {
        fprintf(stderr, "ANV direct ICD enumeration=%d count=%u\n", result, count);
        destroy_instance(instance, NULL);
        dlclose(module);
        return 1;
    }

    destroy_instance(instance, NULL);
    dlclose(module);
    return 0;
}

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
    if (result != VK_SUCCESS || instance == VK_NULL_HANDLE) {
        fprintf(stderr, "vkCreateInstance unexpected VkResult=%d\n", result);
        return 1;
    }

    uint32_t count = 0;
    result = vkEnumeratePhysicalDevices(instance, &count, NULL);
    if ((result != VK_SUCCESS && result != VK_ERROR_INITIALIZATION_FAILED) || count != 0) {
        fprintf(stderr, "expected zero physical devices, loader enumerate=%d count=%u\n",
                result, count);
        vkDestroyInstance(instance, NULL);
        return 1;
    }
    vkDestroyInstance(instance, NULL);

    if (direct_anv_no_device(&create) != 0)
        return 1;

    printf("THEKERNEL_N305_ANV_NO_DEVICE_OK loader_enumerate=%s loader_count=0 "
           "anv_enumerate=VK_SUCCESS anv_count=0 renderer=not_tested\n",
           result == VK_SUCCESS ? "VK_SUCCESS" : "VK_ERROR_INITIALIZATION_FAILED");
    return 0;
}
