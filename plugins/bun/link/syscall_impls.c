// Functional wasi impls for syscalls the runtime calls at startup (uSockets loop).
#include <unistd.h>
#include <errno.h>
// eventfd stand-in: wasip2 has no eventfd and pipe() isn't wired under wasmtime,
// so hand back a dup of stderr — a valid fd the epoll backend can register.
// Cross-thread wakeups never fire in a single-threaded wasm loop, so the fd is
// only ever held, never meaningfully read/written.
int eventfd(unsigned int initval, int flags) {
    (void)initval; (void)flags;
    int fd = dup(2);
    if (fd < 0) { errno = EMFILE; return -1; }
    return fd;
}
// WebView teardown hooks fire on every VM exit; there is no native webview on
// wasi, so they are genuine no-ops (not the trapping stubs the rest of the
// inert webview surface gets).
void Bun__WebView__closeAllForTermination(void) {}
void Bun__WebViewHost__childDied(int died) { (void)died; }
// bun_epoll_pwait2 probes the epoll_pwait2 syscall first; there's none on wasi,
// so return -ENOSYS to make it fall back to the epoll_pwait shim (which sleeps).
#include <errno.h>
long sys_epoll_pwait2(int epfd, void* ev, int maxev, const void* to, const void* mask) {
    (void)epfd;(void)ev;(void)maxev;(void)to;(void)mask; return -ENOSYS;
}
// libuv monotonic high-resolution clock (nanoseconds). node:http's
// JSConnectionsList calls uv_hrtime() for per-connection timing and expects a
// real uint64_t; the QUIC blind stub declared it () -> void, which LLD turned
// into a signature_mismatch trap. wasip2 has a working monotonic clock.
#include <time.h>
#include <stdint.h>
uint64_t uv_hrtime(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}
