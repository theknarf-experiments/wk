// Minimal epoll shim for wasi. There is no epoll and no real socket I/O yet, but
// bun's event loop folds the next-timer expiry into the epoll_pwait timeout (no
// timerfd), so a wait that actually SLEEPS for that timeout — advancing the
// monotonic clock and then returning zero ready fds — is enough to make timers
// (setTimeout/setInterval) and the microtask/timer event loop work.
#ifndef _WASI_COMPAT_SYS_EPOLL_H
#define _WASI_COMPAT_SYS_EPOLL_H
#include <stdint.h>
#include <errno.h>
#include <time.h>
#include <unistd.h>
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
// Return a real (dup'd) fd so the loop has a valid, closeable epoll handle.
static inline int epoll_create1(int f){(void)f;int fd=dup(2);return fd<0?3:fd;}
// Pretend registration succeeds; there are no real fds to poll yet.
static inline int epoll_ctl(int e,int o,int fd,struct epoll_event*ev){(void)e;(void)o;(void)fd;(void)ev;return 0;}
// Sleep for the timeout (ms), then report zero ready fds. t<0 (infinite) has no
// I/O source to wake it here, so return immediately rather than block forever.
static inline int epoll_wait(int e,struct epoll_event*ev,int m,int t){
    (void)e;(void)ev;(void)m;
    if(t>0){struct timespec ts={.tv_sec=t/1000,.tv_nsec=(long)(t%1000)*1000000L};nanosleep(&ts,0);}
    return 0;
}
static inline int epoll_pwait(int e,struct epoll_event*ev,int m,int t,const void*sig){(void)sig;return epoll_wait(e,ev,m,t);}
#ifdef __cplusplus
}
#endif
#endif
