/* Verify dynamically loaded runtime dependencies without requiring a GPU. */
#include <dlfcn.h>
#include <gio/gio.h>
#include <stdio.h>
#ifdef LUMINA_DESKTOP
#include <fontconfig/fontconfig.h>
#include <xkbcommon/xkbcommon.h>
#include <xkbcommon/xkbcommon-compose.h>
#endif

static int load_symbol(const char *library, const char *symbol) {
    void *handle = dlopen(library, RTLD_NOW | RTLD_LOCAL);
    if (!handle || !dlsym(handle, symbol)) {
        fprintf(stderr, "%s unavailable: %s\n", library, dlerror());
        if (handle) dlclose(handle);
        return 0;
    }
    dlclose(handle);
    return 1;
}

int main(void) {
    if (!load_symbol("libvulkan.so.1", "vkGetInstanceProcAddr")) return 1;
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
#ifdef LUMINA_DESKTOP
    if (!load_symbol("libwayland-client.so.0", "wl_display_connect") ||
        !load_symbol("libxcb.so.1", "xcb_connect")) return 1;
    struct xkb_context *context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    struct xkb_keymap *keymap = context ? xkb_keymap_new_from_names(context, NULL, XKB_KEYMAP_COMPILE_NO_FLAGS) : NULL;
    struct xkb_compose_table *compose = context ? xkb_compose_table_new_from_locale(context, "en_US.UTF-8", XKB_COMPOSE_COMPILE_NO_FLAGS) : NULL;
    if (!keymap || !compose) {
        fprintf(stderr, "Keyboard layout or compose data unavailable\n");
        return 1;
    }
    xkb_compose_table_unref(compose);
    xkb_keymap_unref(keymap);
    xkb_context_unref(context);
    FcConfig *config = FcInitLoadConfigAndFonts();
    FcFontSet *fonts = config ? FcConfigGetFonts(config, FcSetSystem) : NULL;
    if (!fonts || fonts->nfont == 0) {
        fprintf(stderr, "Font configuration has no usable fonts\n");
        if (config) FcConfigDestroy(config);
        return 1;
    }
    FcConfigDestroy(config);
    puts("Display client loaders, keyboard data and fonts available");
#endif
    puts("Vulkan loader and GIO TLS backend available");
    return 0;
}
