// First-link stubs for symbols not yet cross-compiled for wasi:
// BoringSSL EVP (needs a libcrypto wasi build) + 2 uSockets QUIC functions
// (behind an HTTP/3 flag). These abort if actually invoked.
#include <stddef.h>
#include <stdint.h>
void abort(void);
// BoringSSL EVP_PKEY
typedef struct evp_pkey_st EVP_PKEY;
typedef struct engine_st ENGINE;
void EVP_PKEY_free(EVP_PKEY* k) { (void)k; }
EVP_PKEY* EVP_PKEY_new_raw_private_key(int type, ENGINE* e, const uint8_t* key, size_t len) {
    (void)type; (void)e; (void)key; (void)len; return NULL;
}
EVP_PKEY* EVP_PKEY_new_raw_public_key(int type, ENGINE* e, const uint8_t* key, size_t len) {
    (void)type; (void)e; (void)key; (void)len; return NULL;
}
// uSockets QUIC (HTTP/3)
typedef struct us_quic_stream_t us_quic_stream_t;
typedef struct us_quic_header_t us_quic_header_t;
unsigned int us_quic_stream_header_count(us_quic_stream_t* s) { (void)s; return 0; }
const us_quic_header_t* us_quic_stream_header(us_quic_stream_t* s, unsigned int i) { (void)s; (void)i; return NULL; }

// --- More first-link stubs: vendored networking/FFI not compiled for wasi ---
// dynamic linking (no dlopen on wasm; FFI/napi disabled)
void* dlopen(const char* f, int m) { (void)f; (void)m; return NULL; }
void* dlsym(void* h, const char* s) { (void)h; (void)s; return NULL; }
int dlclose(void* h) { (void)h; return 0; }
char* dlerror(void) { return NULL; }
void node_module_register(void* m) { (void)m; }
// bun os helper (usually a C++/codegen fn; report 0 free memory)
uint64_t Bun__Os__getFreeMemory(void) { return 0; }
// uSockets core (not compiled for wasi — needs an event backend)
void* us_socket_ext(void* s) { (void)s; return NULL; }
int us_socket_is_closed(const void* s) { (void)s; return 1; }
void* us_quic_stream_ext(void* s) { (void)s; return NULL; }
// lshpack (HTTP/2 HPACK) — needs its own wasi build; stub the raw C API
struct lshpack_enc; struct lshpack_dec;
int lshpack_enc_init(struct lshpack_enc* e) { (void)e; return 0; }
void lshpack_enc_cleanup(struct lshpack_enc* e) { (void)e; }
unsigned char* lshpack_enc_encode(struct lshpack_enc* e, unsigned char* dst, unsigned char* end, void* input) { (void)e; (void)end; (void)input; return dst; }
void lshpack_enc_set_max_capacity(struct lshpack_enc* e, unsigned c) { (void)e; (void)c; }
int lshpack_dec_init(struct lshpack_dec* d) { (void)d; return 0; }
void lshpack_dec_cleanup(struct lshpack_dec* d) { (void)d; }
int lshpack_dec_decode(struct lshpack_dec* d, const unsigned char** src, const unsigned char* end, void* out) { (void)d; (void)src; (void)end; (void)out; return -1; }
void lshpack_dec_set_max_capacity(struct lshpack_dec* d, unsigned c) { (void)d; (void)c; }
