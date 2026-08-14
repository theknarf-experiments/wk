// Socket message/option macros wasi-libc gates out; uSockets references them.
// Inert on wasm (no real sockets) but must compile.
#ifndef _WASI_SOCKET_COMPAT_H
#define _WASI_SOCKET_COMPAT_H
#if defined(__wasi__)
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
// Pre-empt wasi-libc's member-less struct sockaddr_un (WASI "has no unix
// domain sockets") with the full BSD shape, so uSockets bsd.c's sun_path
// code compiles. Claim the header's include guard so its version is skipped.
#include <__typedef_sa_family_t.h>
#ifndef __wasilibc___struct_sockaddr_un_h
#define __wasilibc___struct_sockaddr_un_h
struct sockaddr_un { sa_family_t sun_family; char sun_path[108]; };
#endif
#include <sys/socket.h>
#include <stddef.h>
#ifndef SO_LINGER
#define SO_LINGER 13
#endif
#ifndef SO_REUSEADDR
#define SO_REUSEADDR 2
#endif
#ifndef SO_REUSEPORT
#define SO_REUSEPORT 15
#endif
#ifndef TCP_NODELAY
#define TCP_NODELAY 1
#endif
#ifndef MSG_NOSIGNAL
#define MSG_NOSIGNAL 0x4000
#endif
#ifndef AF_UNIX
#define AF_UNIX 1
#endif
#ifndef SO_BROADCAST
#define SO_BROADCAST 6
#endif
#ifndef SO_KEEPALIVE
#define SO_KEEPALIVE 9
#endif
#ifndef SO_RCVBUF
#define SO_RCVBUF 8
#endif
#ifndef SO_SNDBUF
#define SO_SNDBUF 7
#endif
#ifndef SO_ERROR
#define SO_ERROR 4
#endif
#ifndef SOL_SOCKET
#define SOL_SOCKET 1
#endif
#ifndef SCM_RIGHTS
#define SCM_RIGHTS 1
#endif
#ifndef SOL_IP
#define SOL_IP 0
#endif
#ifndef IP_TOS
#define IP_TOS 1
#endif
#ifndef IPPROTO_TCP
#define IPPROTO_TCP 6
#endif
#ifdef __cplusplus
extern "C" {
#endif
#ifdef __cplusplus
}
#endif
#ifndef struct_cmsghdr_defined
struct cmsghdr { size_t cmsg_len; int cmsg_level; int cmsg_type; };
#endif
#ifdef __cplusplus
extern "C" {
#endif
#ifdef __cplusplus
}
#endif
#ifdef __cplusplus
extern "C" {
#endif
static inline int socketpair(int d, int t, int p, int sv[2]) { (void)d;(void)t;(void)p;(void)sv; return -1; }
static inline long recvmsg(int fd, struct msghdr* m, int flags) { (void)fd;(void)m;(void)flags; return -1; }
static inline long sendmsg(int fd, const struct msghdr* m, int flags) { (void)fd;(void)m;(void)flags; return -1; }
#ifdef __cplusplus
}
#endif
#ifndef CMSG_DATA
#define CMSG_ALIGN(len) (((len) + sizeof(size_t) - 1) & (size_t) ~(sizeof(size_t) - 1))
#define CMSG_DATA(cmsg) ((unsigned char*)((struct cmsghdr*)(cmsg) + 1))
#define CMSG_SPACE(len) (CMSG_ALIGN(sizeof(struct cmsghdr)) + CMSG_ALIGN(len))
#define CMSG_LEN(len) (CMSG_ALIGN(sizeof(struct cmsghdr)) + (len))
#define CMSG_FIRSTHDR(mhdr) ((mhdr)->msg_controllen >= sizeof(struct cmsghdr) ? (struct cmsghdr*)(mhdr)->msg_control : (struct cmsghdr*)0)
#define CMSG_NXTHDR(mhdr, cmsg) ((struct cmsghdr*)0)
#endif
#endif
#endif
