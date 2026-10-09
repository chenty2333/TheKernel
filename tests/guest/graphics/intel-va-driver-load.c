/* Load the target iHD VA-API module without requiring a physical GPU. */
#include <dlfcn.h>
#include <stdio.h>

int main(void)
{
    void *driver = dlopen("/usr/lib/dri/iHD_drv_video.so", RTLD_NOW | RTLD_LOCAL);
    if (driver == NULL) {
        fprintf(stderr, "iHD dlopen failed: %s\n", dlerror());
        return 1;
    }
    puts("THEKERNEL_N305_IHD_MODULE_LOADED");
    dlclose(driver);
    return 0;
}
