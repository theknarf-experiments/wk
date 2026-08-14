// Minimal epoll stub for wasi (no epoll; usockets uses a wasm event backend at runtime).
#ifndef _WASI_COMPAT_SYS_EPOLL_H
#define _WASI_COMPAT_SYS_EPOLL_H
#include <stdint.h>
#include <errno.h>
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
static inline int epoll_create1(int f){(void)f;errno=ENOSYS;return -1;}
static inline int epoll_ctl(int e,int o,int fd,struct epoll_event*ev){(void)e;(void)o;(void)fd;(void)ev;errno=ENOSYS;return -1;}
static inline int epoll_wait(int e,struct epoll_event*ev,int m,int t){(void)e;(void)ev;(void)m;(void)t;errno=ENOSYS;return -1;}
static inline int epoll_pwait(int e,struct epoll_event*ev,int m,int t,const void*sig){(void)e;(void)ev;(void)m;(void)t;(void)sig;errno=ENOSYS;return -1;}
#ifdef __cplusplus
}
#endif
#endif
