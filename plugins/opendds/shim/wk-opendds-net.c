/* wk-opendds-net.c — sendmsg() and recvmsg() for wasm32-wasip2.
 *
 * WHY THIS FILE EXISTS
 * ====================
 * wasi-libc keeps both prototypes inside `#ifdef __wasilibc_unmodified_upstream`
 * — never defined — so on this target they are neither declared nor defined.
 * shim/include/wk-wasi-net-compat.h supplies the declarations; this file
 * supplies the code.
 *
 * They cannot simply be left out. OpenDDS's rtps_udp transport is built on
 * exactly this pair:
 *
 *   - an RTPS message is a header plus N submessages held in SEPARATE buffers,
 *     and OpenDDS hands the lot to ACE_SOCK_Dgram::send(iovec[], n, addr),
 *     which is one sendmsg;
 *   - every receive needs the SENDER's address alongside the payload, because
 *     that is how a participant learns where a peer's unicast locators really
 *     are, and that is one recvmsg.
 *
 * ACE does offer ACE_LACKS_SENDMSG / ACE_LACKS_RECVMSG, and they are the wrong
 * answer: they make ACE_OS::sendmsg/recvmsg `ACE_NOTSUP_RETURN (-1)` with no
 * emulation at all, so the transport would link, run, and move no data.
 *
 * THE EMULATION
 * =============
 * Over sendto/recvfrom, which wasi-libc does implement and which under wk
 * terminate in wk's userspace smoltcp fabric. The same approach, for the same
 * reason, as plugins/bun/link/syscall_impls.c's sendmmsg/recvmmsg.
 *
 * A datagram is atomic, so the iovec has to be flattened into one buffer
 * rather than sent piecewise — N sendto calls would be N datagrams, which is
 * not what the caller asked for and would corrupt every multi-submessage RTPS
 * message. The gather buffer is on the stack and bounded by WK_DGRAM_MAX; the
 * link line gives these binaries an 8 MB shadow stack (platform_wasi.GNU), so
 * 64 KB is affordable, and 64 KB is also the largest a UDP datagram can be.
 *
 * WHAT IS NOT EMULATED, AND WHY THAT IS FINE
 * ------------------------------------------
 * Ancillary data. Both directions set msg_controllen = 0, because recvfrom
 * cannot produce control messages and sendto cannot carry them. The one place
 * ACE cares is ACE_SOCK_Dgram::recv(iov, n, addr, flags, to_addr), which reads
 * IP_PKTINFO out of the control buffer to learn the DESTINATION address of the
 * datagram. With controllen 0, CMSG_FIRSTHDR returns NULL — see
 * include/wk-wasi-net-compat.h — the walk runs zero times, and ACE falls back
 * to get_local_addr(), which is the answer it wanted. The SOURCE address, the
 * one RTPS actually needs, comes from msg_name and is filled in below.
 *
 * MSG_* flags are dropped. wasi-libc's sendto/recvfrom reject any nonzero
 * flags with ENOTSUP, since wasi:sockets has no such concept; the ones ACE
 * passes (MSG_DONTWAIT, MSG_NOSIGNAL) describe behaviour a wasm guest has
 * anyway — the socket's blocking mode is set with fcntl, and there are no
 * signals. Again the same finding as plugins/bun.
 */

#include <errno.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include <wk-wasi-net-compat.h>

/* WK_DDS_TRACE=1 reports every recvmsg with the socket's blocking mode. A
 * blocking recv with no data waits forever on this runtime (there is no other
 * thread to deliver it), so when a node hangs, the last line this prints names
 * the descriptor to look at. */
static void wk_trace_recv (int fd)
{
    static int on = -1;
    char b[96];
    int n, fl;
    if (on < 0)
        on = getenv ("WK_DDS_TRACE") != 0;
    if (!on)
        return;
    fl = fcntl (fd, F_GETFL, 0);
    n = snprintf (b, sizeof b, "[wk] recvmsg fd=%d nonblock=%d\n",
                  fd, fl >= 0 && (fl & O_NONBLOCK) ? 1 : 0);
    (void)!write (2, b, (size_t) n);
}

/* The largest a UDP datagram can be (65535 minus the 8-byte UDP header and the
 * 20-byte IPv4 header), rounded up to a whole 64 KB for the gather buffer. */
#define WK_DGRAM_MAX 65536

ssize_t sendmsg (int fd, const struct msghdr *msg, int flags)
{
    unsigned char gather[WK_DGRAM_MAX];
    const void *buf;
    size_t len = 0;
    int i;

    (void)flags; /* see the header comment: wasip2 rejects nonzero flags */

    if (msg == NULL) {
        errno = EINVAL;
        return -1;
    }

    if (msg->msg_iovlen == 1) {
        /* The common case, and worth not copying: OpenDDS's control messages
         * and any single-buffer send land here. */
        buf = msg->msg_iov[0].iov_base;
        len = msg->msg_iov[0].iov_len;
    } else {
        for (i = 0; i < (int)msg->msg_iovlen; i++) {
            size_t n = msg->msg_iov[i].iov_len;
            if (n > sizeof gather - len) {
                /* Truncating would send a corrupt RTPS message that the peer
                 * would try to parse. Refuse instead — EMSGSIZE is what a real
                 * kernel says for an oversized datagram, and ACE surfaces it. */
                errno = EMSGSIZE;
                return -1;
            }
            memcpy (gather + len, msg->msg_iov[i].iov_base, n);
            len += n;
        }
        buf = gather;
    }

    return sendto (fd, buf, len, 0,
                   (const struct sockaddr *)msg->msg_name, msg->msg_namelen);
}

ssize_t recvmsg (int fd, struct msghdr *msg, int flags)
{
    unsigned char scatter[WK_DGRAM_MAX];
    socklen_t namelen;
    ssize_t got;
    size_t left, off;
    int i;

    (void)flags;

    if (msg == NULL) {
        errno = EINVAL;
        return -1;
    }

    namelen = msg->msg_namelen;
    wk_trace_recv (fd);

    if (msg->msg_iovlen == 1) {
        got = recvfrom (fd, msg->msg_iov[0].iov_base, msg->msg_iov[0].iov_len,
                        0, (struct sockaddr *)msg->msg_name, &namelen);
    } else {
        /* One datagram into one buffer, then split across the iovec. Reading
         * directly into the first iov and then asking for the rest would lose
         * everything past the first buffer: a second recvfrom would take the
         * NEXT datagram, not the remainder of this one. */
        got = recvfrom (fd, scatter, sizeof scatter, 0,
                        (struct sockaddr *)msg->msg_name, &namelen);
        if (got > 0) {
            left = (size_t)got;
            off = 0;
            for (i = 0; i < (int)msg->msg_iovlen && left > 0; i++) {
                size_t n = msg->msg_iov[i].iov_len;
                if (n > left)
                    n = left;
                memcpy (msg->msg_iov[i].iov_base, scatter + off, n);
                off += n;
                left -= n;
            }
            /* A datagram larger than the caller's buffers is truncated, and
             * POSIX says to report that in msg_flags rather than as an error.
             * ACE checks this for its "peek at the size" paths. */
            if (left > 0)
                msg->msg_flags |= MSG_TRUNC;
        }
    }

    if (got < 0)
        return -1;

    msg->msg_namelen = namelen;
    /* No ancillary data is possible over recvfrom. Saying so explicitly is
     * what makes ACE's CMSG_FIRSTHDR walk a no-op rather than a walk over
     * whatever the caller's control buffer happened to contain. */
    msg->msg_control = NULL;
    msg->msg_controllen = 0;

    return got;
}
