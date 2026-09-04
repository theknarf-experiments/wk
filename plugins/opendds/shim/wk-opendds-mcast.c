/* wk-opendds-mcast.c — the multicast socket options, on a fabric where
 * membership is implicit.
 *
 * WHY THESE CAN SUCCEED
 * =====================
 * POSIX makes IP_ADD_MEMBERSHIP mandatory for RECEIVING a group, and the
 * reasons are all about hardware and network gear that wk's fabric does not
 * have:
 *
 *   - the NIC filters by MAC, so it must be programmed to accept the group's
 *     multicast MAC address. There is no NIC here and no MAC: the fabric's
 *     medium is raw IP (smoltcp `Medium::Ip`), with no Ethernet header at all.
 *   - a switch or router must be told to forward the group to this port, which
 *     is what the IGMP membership report is for. The fabric's Network is a hub
 *     that already sees every frame from every member; there is nothing to
 *     inform.
 *   - the kernel must accept a packet not addressed to one of its own
 *     addresses. That one is real -- and it is wk's decision to make, because
 *     each node's "kernel" is a smoltcp instance the HOST owns. The hub joins
 *     the group on the node's behalf when it first routes one (see
 *     crates/wk-fabric/src/netstack.rs).
 *
 * So a group arrives whether or not the guest asked for it, and the honest
 * return for "please let me receive group G" is success.
 *
 * WHY IT HAS TO BE SAID AT ALL, RATHER THAN JUST WORKING
 * ------------------------------------------------------
 * Because callers CHECK. ACE_SOCK_Dgram_Mcast::subscribe_ifs() returns -1 if
 * the setsockopt fails, OpenDDS's MulticastManager::join() then records no
 * joined interface, and Spdp never uses its multicast socket -- so discovery
 * silently falls back to whatever unicast configuration it was given. The
 * datagrams would be arriving at the socket and nobody would be reading them.
 *
 * The transmit-side options (TTL, LOOP, IF) are accepted for the same reason
 * in reverse: they configure a send that needs no configuring here. A TTL is a
 * hop budget for routers the fabric does not have; LOOP defaults to on and the
 * hub always copies to the sender; IF selects among interfaces a node does not
 * have. Refusing them made OpenDDS treat participant creation as failed
 * (Spdp::SpdpTransport::set_unicast_socket_opts throws on a TTL it cannot set)
 * even though nothing was actually wrong.
 *
 * WHY --wrap
 * ==========
 * Every OTHER socket option must still reach wasi-libc: SO_RCVBUF, SO_REUSEADDR
 * and TCP_NODELAY are all real and all used. A plain override could not call
 * the function it replaced, so the link line uses
 *
 *     -Wl,--wrap=setsockopt
 *
 * which renames the libc symbol to __real_setsockopt and points every caller at
 * __wrap_setsockopt below. That is the one mechanism that lets a shim
 * INTERPOSE rather than replace.
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <netinet/in.h>
#include <sys/socket.h>

/* For SO_REUSEPORT, which wasi-libc keeps inside its
 * `#ifdef __wasilibc_unmodified_upstream` block along with five other SO_*
 * constants -- so it is not merely unimplemented but undeclared. Our header
 * supplies it (with wasi-libc's own value). */
#include <wk-wasi-net-compat.h>

/* Provided by lld under --wrap: the real wasi-libc setsockopt. */
int __real_setsockopt (int fd, int level, int optname,
                       const void *optval, socklen_t optlen);

int __wrap_setsockopt (int fd, int level, int optname,
                       const void *optval, socklen_t optlen)
{
    /* SO_REUSEPORT: nothing to relax, so say yes.
     *
     * It asks the stack to let several sockets share one port, which matters
     * when several participants run on one host. On the fabric each node IS a
     * host -- its own address, its own stack -- so nothing else is contending
     * for the port and the hint has no work to do. wasi-libc refuses it
     * (ENOPROTOOPT), and ACE_SOCK_Dgram_Mcast::open_i() treats that refusal as
     * fatal, so the multicast socket never opens and the join fails several
     * frames later with a stale errno from somewhere else entirely. That is
     * the single option standing between this port and stock RTPS discovery.
     *
     * Two sockets in ONE node still cannot share a port -- the bind fails, as
     * it would anywhere -- so this grants nothing that was not already true. */
    if (level == SOL_SOCKET && optname == SO_REUSEPORT) {
        return 0;
    }

    if (level == IPPROTO_IP) {
        switch (optname) {
        case IP_ADD_MEMBERSHIP:   /* receiving: the hub already delivers */
        case IP_DROP_MEMBERSHIP:
        case IP_MULTICAST_TTL:    /* sending: nothing to configure */
        case IP_MULTICAST_LOOP:
        case IP_MULTICAST_IF:
            return 0;
        default:
            break;
        }
    }

#ifdef IPPROTO_IPV6
    if (level == IPPROTO_IPV6) {
        switch (optname) {
#ifdef IPV6_JOIN_GROUP
        case IPV6_JOIN_GROUP:
#endif
#ifdef IPV6_LEAVE_GROUP
        case IPV6_LEAVE_GROUP:
#endif
#ifdef IPV6_MULTICAST_HOPS
        case IPV6_MULTICAST_HOPS:
#endif
#ifdef IPV6_MULTICAST_LOOP
        case IPV6_MULTICAST_LOOP:
#endif
#ifdef IPV6_MULTICAST_IF
        case IPV6_MULTICAST_IF:
#endif
            return 0;
        default:
            break;
        }
    }
#endif

    {
        /* Everything else is wasi-libc's. WK_DDS_TRACE reports the ones it
         * REFUSES, because a refused socket option is a common way for a
         * library to abandon a whole feature quietly -- which is exactly how
         * SO_REUSEPORT above hid stock multicast discovery for a while. */
        const int r = __real_setsockopt (fd, level, optname, optval, optlen);
        if (r < 0 && getenv ("WK_DDS_TRACE")) {
            char b[96];
            int n = snprintf (b, sizeof b,
                              "[wk] setsockopt refused: level=%d opt=%d errno=%d\n",
                              level, optname, errno);
            (void)!write (2, b, (size_t) n);
        }
        return r;
    }
}
