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
void Bun__WebViewHost__childDied(void) {}
