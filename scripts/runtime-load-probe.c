/* Verify dynamically loaded runtime dependencies without requiring a GPU. */
#include <dlfcn.h>
#include <gio/gio.h>
#include <stdio.h>

int main(void) {
    void *vulkan = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!vulkan || !dlsym(vulkan, "vkGetInstanceProcAddr")) {
        fprintf(stderr, "Vulkan loader unavailable: %s\n", dlerror());
        if (vulkan) dlclose(vulkan);
        return 1;
    }
    dlclose(vulkan);
    GTlsBackend *backend = g_tls_backend_get_default();
    if (!g_tls_backend_supports_tls(backend) ||
        !g_tls_backend_get_default_database(backend)) {
        fprintf(stderr, "GIO TLS backend or trust database unavailable\n");
        return 1;
    }
    const char *certificates = g_getenv("SSL_CERT_FILE");
    if (!certificates || !g_file_test(certificates, G_FILE_TEST_IS_REGULAR)) {
        fprintf(stderr, "Private CA trust store unavailable\n");
        return 1;
    }
    puts("Vulkan loader and GIO TLS backend available");
    return 0;
}
