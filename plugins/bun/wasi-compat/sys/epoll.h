// epoll shim for wasi, implemented over poll(2) (wasi-libc's poll_oneoff, which
// works for socket fds backed by wasi:sockets). epoll_ctl records each fd's
// requested events + data; epoll_pwait polls them and translates readiness back
// to epoll_events — so bun's uSockets event loop actually learns when sockets
// are readable/writable (HTTP serve/fetch work in a wk node wired to a Network).
// With no fds registered it just honors the timeout, which is how the timer
// wait (setTimeout/setInterval) is driven. Definitions live in one translation
// unit (link/epoll_impl.c) because the fd table is shared process-wide state.
#ifndef _WASI_COMPAT_SYS_EPOLL_H
#define _WASI_COMPAT_SYS_EPOLL_H
#include <stdint.h>
typedef union epoll_data { void *ptr; int fd; uint32_t u32; uint64_t u64; } epoll_data_t;
struct epoll_event { uint32_t events; epoll_data_t data; };
#define EPOLLIN 0x001
#define EPOLLPRI 0x002
#define EPOLLOUT 0x004
#define EPOLLERR 0x008
#define EPOLLHUP 0x010
#define EPOLLRDHUP 0x2000
#define EPOLLET (1u<<31)
#define EPOLLONESHOT (1u<<30)
#define EPOLL_CTL_ADD 1
#define EPOLL_CTL_DEL 2
#define EPOLL_CTL_MOD 3
#define EPOLL_CLOEXEC 02000000
#ifdef __cplusplus
extern "C" {
#endif
int epoll_create1(int flags);
int epoll_ctl(int epfd, int op, int fd, struct epoll_event *ev);
int epoll_wait(int epfd, struct epoll_event *events, int maxevents, int timeout);
int epoll_pwait(int epfd, struct epoll_event *events, int maxevents, int timeout,
                const void *sigmask);
#ifdef __cplusplus
}
#endif
#endif
