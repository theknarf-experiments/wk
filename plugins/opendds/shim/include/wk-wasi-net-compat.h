/* wk-wasi-net-compat.h — the socket declarations wasi-libc withholds.
 *
 * Included from ace/config-wasi.h, which ace/config.h includes at the top of
 * every ACE, TAO and OpenDDS translation unit. On the include path via
 * platform_wasi.GNU's -I.
 *
 * WHY A HEADER RATHER THAN PATCHES
 * ================================
 * wasi-libc's <sys/socket.h> keeps struct cmsghdr, the whole CMSG_* accessor
 * family and the sendmsg/recvmsg prototypes inside
 *
 *     #ifdef __wasilibc_unmodified_upstream
 *
 * which is never defined — so they are not merely unimplemented, they are not
 * DECLARED. Meanwhile <netinet/in.h> does define IP_PKTINFO, and <sys/ioctl.h>
 * does define SIOCGIFCONF and SIOCGIFADDR. The result is a libc that says
 * "this platform supports ancillary data and interface ioctls" through its
 * constants and then supplies none of the types to use them with.
 *
 * ACE reads those constants and compiles the corresponding code —
 * SOCK_Dgram.cpp's IP_PKTINFO branch, ACE::get_ip_interfaces' SIOCGIFCONF
 * branch — and then fails on the missing types. Carving those branches out of
 * ACE would take patches in several files and would be the wrong shape: the
 * branches are not wrong, the declarations are missing.
 *
 * So supply the declarations. What then happens at RUN time is correct without
 * anyone pretending:
 *
 *   - wk-opendds-net.c's recvmsg sets msg_controllen = 0 (it is built on
 *     recvfrom, which has no ancillary data), so CMSG_FIRSTHDR — whose own
 *     definition is `controllen >= sizeof(cmsghdr) ? ... : 0` — returns NULL
 *     and ACE's walk over the control messages runs zero times. ACE then falls
 *     back to get_local_addr() for the destination address, which is the
 *     answer it wanted.
 *   - ioctl(SIOCGIFCONF) reaches wasi-libc's ioctl, which knows only FIONREAD
 *     and FIONBIO and fails everything else, so ACE::get_ip_interfaces()
 *     reports no interfaces. That is the truth about a wasm guest: its address
 *     on wk's fabric is assigned by the host, not owned by it, and OpenDDS is
 *     told the address explicitly. See ../PORTING.md, "Which address am I".
 *
 * The layouts below are musl's — the ones wasi-libc itself would have used had
 * the block not been compiled out — so nothing here disagrees with the libc
 * next to it.
 */

#ifndef WK_WASI_NET_COMPAT_H
#define WK_WASI_NET_COMPAT_H

#include <sys/socket.h>   /* struct msghdr, socklen_t, ssize_t */

#ifdef __cplusplus
extern "C" {
#endif

/* --- socket options that fell inside the same compiled-out block ---------- */

/* Six of the SO_* constants sit in wasi-libc's `__wasilibc_unmodified_upstream`
 * region while the rest (SO_REUSEADDR, SO_RCVBUF, SO_SNDBUF, SO_KEEPALIVE,
 * SO_ERROR, and every IP_*, IPV6_* and TCP_* one) are visible — a split that
 * looks arbitrary from outside and is really just where the file happened to
 * be cut. Established by compile-probing all 31 constants ACE, TAO and OpenDDS
 * reference, not by reading the guards.
 *
 * The values are wasi-libc's own, copied from that region, so a future SDK
 * which enables the block defines exactly these and the #ifndefs stop firing.
 *
 * Defining them does not make them work, and does not need to: wasi:sockets
 * has no such options, so setsockopt returns ENOTSUP. Every caller here treats
 * these as advisory tuning — TAO's IIOP connection handler sets SO_DONTROUTE
 * and ignores the result, ACE's ACE_SOCK_Dgram_Bcast asks for SO_BROADCAST —
 * and none of them is on the RTPS path this port exists for. Without the
 * definitions the code does not COMPILE, which is a much worse failure than
 * an option that politely declines.
 */
#ifndef SO_DEBUG
#define SO_DEBUG      1
#endif
#ifndef SO_DONTROUTE
#define SO_DONTROUTE  5
#endif
#ifndef SO_BROADCAST
#define SO_BROADCAST  6
#endif
#ifndef SO_OOBINLINE
#define SO_OOBINLINE  10
#endif
#ifndef SO_LINGER
#define SO_LINGER     13
#endif
#ifndef SO_REUSEPORT
#define SO_REUSEPORT  15
#endif

/* --- scheduling policies -------------------------------------------------- */

/* wasi-libc's <sched.h> declares sched_yield and sched_get_priority_min/max
 * but none of the POLICY constants, because there is nothing to schedule.
 * TAO names them anyway — TAO_ORB_Core's thread parameters carry a policy
 * field, and tao/params.cpp initialises it to SCHED_OTHER — so they have to
 * exist for the code to compile even though no thread will ever be created.
 * Linux/musl values; SCHED_OTHER is the "no real-time policy" one, which is
 * the honest answer here.
 *
 * These live in this header rather than a <sched.h> of ours because a shim
 * <sched.h> would shadow wasi-libc's real one and would then have to restate
 * everything it does declare. */
#ifndef SCHED_OTHER
#define SCHED_OTHER 0
#endif
#ifndef SCHED_FIFO
#define SCHED_FIFO  1
#endif
#ifndef SCHED_RR
#define SCHED_RR    2
#endif

/* --- ancillary data ------------------------------------------------------ */

#ifndef CMSG_ALIGN
struct cmsghdr {
    socklen_t cmsg_len;
    int cmsg_level;
    int cmsg_type;
};

/* Verbatim from wasi-libc's own (compiled-out) <sys/socket.h>, so that a
 * future SDK which enables that block defines exactly these and the #ifndef
 * above simply stops firing. */
#define __CMSG_LEN(cmsg)  (((cmsg)->cmsg_len + sizeof(long) - 1) & ~(long)(sizeof(long) - 1))
#define __CMSG_NEXT(cmsg) ((unsigned char *)(cmsg) + __CMSG_LEN(cmsg))
#define __MHDR_END(mhdr)  ((unsigned char *)(mhdr)->msg_control + (mhdr)->msg_controllen)

#define CMSG_DATA(cmsg) ((unsigned char *) (((struct cmsghdr *)(cmsg)) + 1))
#define CMSG_NXTHDR(mhdr, cmsg) ((cmsg)->cmsg_len < sizeof (struct cmsghdr) || \
    __CMSG_LEN(cmsg) + sizeof(struct cmsghdr) >= (size_t)(__MHDR_END(mhdr) - (unsigned char *)(cmsg)) \
    ? 0 : (struct cmsghdr *)__CMSG_NEXT(cmsg))
#define CMSG_FIRSTHDR(mhdr) ((size_t) (mhdr)->msg_controllen >= sizeof (struct cmsghdr) \
    ? (struct cmsghdr *) (mhdr)->msg_control : (struct cmsghdr *) 0)

#define CMSG_ALIGN(len) (((len) + sizeof (size_t) - 1) & (size_t) ~(sizeof (size_t) - 1))
#define CMSG_SPACE(len) (CMSG_ALIGN (len) + CMSG_ALIGN (sizeof (struct cmsghdr)))
#define CMSG_LEN(len)   (CMSG_ALIGN (sizeof (struct cmsghdr)) + (len))
#endif /* !CMSG_ALIGN */

/* --- scatter/gather datagrams -------------------------------------------- */

/* Implemented in ../wk-opendds-net.c over sendto/recvfrom. OpenDDS's rtps_udp
 * transport is built on this pair: an RTPS message is a header plus N
 * submessages in separate buffers, and every receive needs the sender's
 * address alongside the payload. */
ssize_t sendmsg (int, const struct msghdr *, int);
ssize_t recvmsg (int, struct msghdr *, int);

#ifdef __cplusplus
}
#endif

#endif /* WK_WASI_NET_COMPAT_H */
