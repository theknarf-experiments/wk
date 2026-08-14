// First-link stubs for vendored networking/FFI symbols not yet cross-compiled
// for wasi (uSockets core needs an event backend; lshpack needs its build).
// BoringSSL crypto is now REAL (libbssl_crypto.a) — no crypto stubs here.
#include <stddef.h>
#include <stdint.h>
// dynamic linking (no dlopen on wasm)
void* dlopen(const char* f, int m) { (void)f; (void)m; return NULL; }
void* dlsym(void* h, const char* s) { (void)h; (void)s; return NULL; }
int dlclose(void* h) { (void)h; return 0; }
char* dlerror(void) { return NULL; }
void node_module_register(void* m) { (void)m; }
uint64_t Bun__Os__getFreeMemory(void) { return 0; }
// uSockets core (not compiled for wasi — needs an event backend)
void* us_socket_ext(void* s) { (void)s; return NULL; }
int us_socket_is_closed(const void* s) { (void)s; return 1; }
void* us_quic_stream_ext(void* s) { (void)s; return NULL; }
void* us_loop_ext(void* loop) { (void)loop; return NULL; }
int us_socket_write(void* s, const char* data, int length) { (void)s; (void)data; return length; }
void* us_socket_group(void* s) { (void)s; return NULL; }
void* us_socket_group_loop(void* group) { (void)group; return NULL; }
// uSockets QUIC (HTTP/3)
void* us_quic_stream_header_count(void* s) { (void)s; return NULL; }
void* us_quic_stream_header(void* s, unsigned int i) { (void)s; (void)i; return NULL; }
// lshpack (HTTP/2 HPACK) — needs its own build
struct lshpack_enc; struct lshpack_dec;
int lshpack_enc_init(struct lshpack_enc* e) { (void)e; return 0; }
void lshpack_enc_cleanup(struct lshpack_enc* e) { (void)e; }
unsigned char* lshpack_enc_encode(struct lshpack_enc* e, unsigned char* dst, unsigned char* end, void* input) { (void)e; (void)end; (void)input; return dst; }
void lshpack_enc_set_max_capacity(struct lshpack_enc* e, unsigned c) { (void)e; (void)c; }
int lshpack_dec_init(struct lshpack_dec* d) { (void)d; return 0; }
void lshpack_dec_cleanup(struct lshpack_dec* d) { (void)d; }
int lshpack_dec_decode(struct lshpack_dec* d, const unsigned char** src, const unsigned char* end, void* out) { (void)d; (void)src; (void)end; (void)out; return -1; }
void lshpack_dec_set_max_capacity(struct lshpack_dec* d, unsigned c) { (void)d; (void)c; }
