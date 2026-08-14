// Blocking-completion wrapper for connect() on wasi.
//
// wasip2 TCP connect is two-phase: connect() on a non-blocking socket calls
// wasi:sockets `start-connect` and returns EINPROGRESS; completion (`finish-
// connect`, which obtains the socket's I/O streams) is driven by POLLING the
// socket's wasi:io/poll pollable. bun's uSockets uses BSD semantics — after
// EINPROGRESS it waits for a writable epoll event and fires `on_open` WITHOUT
// completing the connect — and our epoll shim can't poll socket fds anyway
// (wasi-libc's poll()/poll_oneoff returns ENOTSUP for them). Result: the TCP
// handshake completes (the peer sees the connection) but the client socket
// never gets its streams, so retrying connect() just returns EALREADY forever
// and every read/write silently goes nowhere.
//
// Fix: do the connect BLOCKING. wasi-libc's blocking connect() drives
// start-connect + wasi:io/poll + finish-connect internally (that internal poll
// waits on the socket's pollable directly, so it works even though app-level
// poll() does not) and returns once the socket has its streams. Restore
// non-blocking afterward so uSockets' subsequent reads/writes behave as it
// expects. This briefly blocks the event loop for the connect handshake (fast
// on the local fabric; bounded by wasi:sockets' own connect timeout).
#include <sys/socket.h>
#include <fcntl.h>
#include <errno.h>

extern int __real_connect(int fd, const struct sockaddr *addr, socklen_t len);

int __wrap_connect(int fd, const struct sockaddr *addr, socklen_t len) {
    int flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0 || !(flags & O_NONBLOCK)) {
        // Already blocking, or flags unavailable — nothing to adjust.
        return __real_connect(fd, addr, len);
    }
    fcntl(fd, F_SETFL, flags & ~O_NONBLOCK); // block for the handshake
    int r = __real_connect(fd, addr, len);
    int e = errno;
    fcntl(fd, F_SETFL, flags); // restore non-blocking for uSockets
    errno = e;
    return r;
}
