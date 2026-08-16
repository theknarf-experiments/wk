// First-link stubs for vendored FFI symbols not cross-compiled for wasi.
// BoringSSL crypto is REAL (libbssl_crypto.a); lshpack (HTTP/2 HPACK) is now
// REAL too (native/lshpack, compiled into /tmp/lshpack.o + /tmp/xxhash.o and
// linked in link_all.sh) — no lshpack stubs here.
#include <stddef.h>
#include <stdint.h>
// dynamic linking (no dlopen on wasm)
void* dlopen(const char* f, int m) { (void)f; (void)m; return NULL; }
void* dlsym(void* h, const char* s) { (void)h; (void)s; return NULL; }
int dlclose(void* h) { (void)h; return 0; }
char* dlerror(void) { return NULL; }
void node_module_register(void* m) { (void)m; }
uint64_t Bun__Os__getFreeMemory(void) { return 0; }
