/* resolv.c — the four libresolv entry points, over BSD sockets. See resolv.h
 * for what this is and why it exists.
 *
 * Everything here is ordinary POSIX: socket/sendto/recvfrom/poll for UDP,
 * connect/send/recv for the TCP retry. On wasm32-wasip2 wasi-libc maps those
 * onto wasi:sockets, which wk's fabric backs — plugins/fetch.c reaches the
 * network the same way — so nothing in this file is wasm-specific. It builds
 * and behaves identically on a desktop, which is how it was tested against a
 * real resolver before being pointed at the fabric.
 */
#include "resolv.h"

#include <arpa/inet.h>
#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

/* A DNS message never exceeds 64K, and the 2-byte TCP length prefix says so. */
#define WK_MAX_DNS 65535

int res_ninit(res_state statp)
{
    if (!statp) {
        errno = EINVAL;
        return -1;
    }
    memset(statp, 0, sizeof(*statp));
    statp->options = RES_DEFAULT | RES_INIT;
    statp->retrans = 5; /* seconds per attempt, glibc's RES_TIMEOUT */
    statp->retry = 2;   /* attempts, glibc's RES_DFLRETRY */
    statp->ndots = 1;

    /* A single `nameserver <ip>` line, if the node's vfs has the file. No
     * search/domain/options handling: see the header. */
    FILE *f = fopen("/etc/resolv.conf", "re");
    if (!f)
        return 0;
    char line[256];
    int nsrch = 0;
    while (fgets(line, sizeof(line), f)) {
        char addr[128];
        /* `domain` and `search` feed defdname/dnsrch, which is all
         * QHostInfo::localDomainName() looks at. `search` wins if both appear,
         * as in every other resolver. */
        if (sscanf(line, " domain %255s", statp->defdname) == 1)
            continue;
        if (strncmp(line, "search", 6) == 0 && (line[6] == ' ' || line[6] == '\t')) {
            char *save = NULL;
            for (char *t = strtok_r(line + 6, " \t\r\n", &save);
                 t && nsrch < MAXDNSRCH; t = strtok_r(NULL, " \t\r\n", &save)) {
                statp->dnsrch[nsrch] = strdup(t);
                if (!statp->dnsrch[nsrch])
                    break;
                ++nsrch;
            }
            statp->dnsrch[nsrch] = NULL;
            if (nsrch > 0 && statp->defdname[0] == '\0')
                snprintf(statp->defdname, sizeof(statp->defdname), "%s", statp->dnsrch[0]);
            continue;
        }
        if (statp->nscount != 0)
            continue;
        if (sscanf(line, " nameserver %127s", addr) != 1)
            continue;
        struct sockaddr_in *sin = &statp->nsaddr_list[0];
        if (inet_pton(AF_INET, addr, &sin->sin_addr) == 1) {
            sin->sin_family = AF_INET;
            sin->sin_port = htons(NS_DEFAULTPORT);
            statp->nscount = 1;
        } else {
            struct sockaddr_in6 *s6 = calloc(1, sizeof(*s6));
            if (s6 && inet_pton(AF_INET6, addr, &s6->sin6_addr) == 1) {
                s6->sin6_family = AF_INET6;
                s6->sin6_port = htons(NS_DEFAULTPORT);
                statp->_u._ext.nsaddrs[0] = s6;
                statp->_u._ext.nscount6 = 1;
            } else {
                free(s6);
            }
        }
    }
    fclose(f);
    return 0;
}

void res_nclose(res_state statp)
{
    if (!statp)
        return;
    /* glibc's res_close() frees the nsaddrs[] entries, and callers allocate
     * into that array precisely because it will (Qt's setIpv6NameServer says
     * so in a comment). Honour the contract or every IPv6 lookup leaks. */
    for (int i = 0; i < MAXNS; ++i) {
        free(statp->_u._ext.nsaddrs[i]);
        statp->_u._ext.nsaddrs[i] = NULL;
    }
    statp->_u._ext.nscount6 = 0;
    statp->nscount = 0;
    for (int i = 0; i < MAXDNSRCH && statp->dnsrch[i]; ++i) {
        free(statp->dnsrch[i]);
        statp->dnsrch[i] = NULL;
    }
}

/* The BIND-era global state. See resolv.h: Qt's qhostinfo_unix.cpp uses it for
 * QHostInfo::localDomainName(). */
struct __res_state _res;

int res_init(void)
{
    return res_ninit(&_res);
}

/* Encode `dname` as a sequence of length-prefixed labels terminated by a root
 * label. A trailing dot is accepted and ignored; an empty name is the root. */
static int encode_qname(const char *dname, unsigned char *out, int outlen)
{
    int w = 0;
    const char *p = dname;
    while (*p) {
        const char *dot = strchr(p, '.');
        size_t len = dot ? (size_t)(dot - p) : strlen(p);
        if (len == 0) {
            /* An empty label is only legal as the trailing root dot. */
            if (dot && dot[1] == '\0')
                break;
            errno = EINVAL;
            return -1;
        }
        if (len > NS_MAXLABEL) {
            errno = EINVAL;
            return -1;
        }
        if (w + 1 + (int)len >= outlen) {
            errno = ENOSPC;
            return -1;
        }
        out[w++] = (unsigned char)len;
        memcpy(out + w, p, len);
        w += (int)len;
        if (!dot)
            break;
        p = dot + 1;
    }
    if (w + 1 > outlen) {
        errno = ENOSPC;
        return -1;
    }
    out[w++] = 0; /* root */
    if (w > NS_MAXCDNAME) {
        errno = EINVAL;
        return -1;
    }
    return w;
}

int res_nmkquery(res_state statp, int op, const char *dname, int class_, int type,
                 const unsigned char *data, int datalen, const unsigned char *newrr_in,
                 unsigned char *buf, int buflen)
{
    (void)newrr_in;
    if (!statp || !dname || !buf || op != ns_o_query || data || datalen) {
        errno = EINVAL;
        return -1;
    }
    if (buflen < NS_HFIXEDSZ) {
        errno = ENOSPC;
        return -1;
    }

    memset(buf, 0, NS_HFIXEDSZ);
    /* Write the header by hand rather than through HEADER's bitfields: the
     * wire layout is fixed and this cannot be got wrong by a compiler's
     * bitfield packing. Bytes 2-3 are the flags; 0x0100 is RD. */
    unsigned short id = (unsigned short)(++statp->id);
    buf[0] = (unsigned char)(id >> 8);
    buf[1] = (unsigned char)(id & 0xff);
    if (statp->options & RES_RECURSE)
        buf[2] = 0x01; /* RD */
    buf[4] = 0;
    buf[5] = 1; /* qdcount = 1 */

    int n = encode_qname(dname, buf + NS_HFIXEDSZ, buflen - NS_HFIXEDSZ);
    if (n < 0)
        return -1;
    int w = NS_HFIXEDSZ + n;
    if (w + 4 > buflen) {
        errno = ENOSPC;
        return -1;
    }
    buf[w++] = (unsigned char)(type >> 8);
    buf[w++] = (unsigned char)(type & 0xff);
    buf[w++] = (unsigned char)(class_ >> 8);
    buf[w++] = (unsigned char)(class_ & 0xff);
    return w;
}

/* The nameserver to talk to, as a sockaddr. IPv6 wins if one is set, matching
 * glibc's preference for the _ext list. Returns 0 if none is configured. */
static socklen_t nameserver_addr(res_state statp, struct sockaddr_storage *ss)
{
    if (statp->_u._ext.nscount6 > 0 && statp->_u._ext.nsaddrs[0]) {
        memcpy(ss, statp->_u._ext.nsaddrs[0], sizeof(struct sockaddr_in6));
        return sizeof(struct sockaddr_in6);
    }
    if (statp->nscount > 0 && statp->nsaddr_list[0].sin_family == AF_INET) {
        memcpy(ss, &statp->nsaddr_list[0], sizeof(struct sockaddr_in));
        return sizeof(struct sockaddr_in);
    }
    return 0;
}

/* One UDP attempt. Returns the reply length, 0 on timeout, -1 on error. */
static int send_udp(const struct sockaddr_storage *ss, socklen_t slen, int timeout_s,
                    const unsigned char *msg, int msglen, unsigned char *answer, int anslen)
{
    int fd = socket(ss->ss_family, SOCK_DGRAM, 0);
    if (fd < 0)
        return -1;

    int rc = -1;
    if (sendto(fd, msg, (size_t)msglen, 0, (const struct sockaddr *)ss, slen) < 0)
        goto out;

    /* Wait for exactly one datagram. A reply whose id does not match the
     * query is ignored and the wait resumes — an off-path forgery or a late
     * answer to a previous query must not be mistaken for this one. */
    for (;;) {
        struct pollfd pfd = { .fd = fd, .events = POLLIN, .revents = 0 };
        int pr = poll(&pfd, 1, timeout_s * 1000);
        if (pr < 0) {
            if (errno == EINTR)
                continue;
            goto out;
        }
        if (pr == 0) {
            rc = 0; /* timeout */
            goto out;
        }
        ssize_t n = recv(fd, answer, (size_t)anslen, 0);
        if (n < 0) {
            if (errno == EINTR)
                continue;
            goto out;
        }
        if (n >= NS_HFIXEDSZ && answer[0] == msg[0] && answer[1] == msg[1]) {
            rc = (int)n;
            goto out;
        }
        /* wrong id -- keep waiting within the same timeout budget */
    }

out:;
    int saved = errno;
    close(fd);
    errno = saved;
    return rc;
}

/* The TCP retry used when a UDP answer comes back truncated. RFC 1035 frames
 * each message with a 2-byte big-endian length. */
static int send_tcp(const struct sockaddr_storage *ss, socklen_t slen,
                    const unsigned char *msg, int msglen, unsigned char *answer, int anslen)
{
    int fd = socket(ss->ss_family, SOCK_STREAM, 0);
    if (fd < 0)
        return -1;

    int rc = -1;
    if (connect(fd, (const struct sockaddr *)ss, slen) < 0)
        goto out;

    unsigned char pfx[2] = { (unsigned char)(msglen >> 8), (unsigned char)(msglen & 0xff) };
    if (send(fd, pfx, 2, 0) != 2)
        goto out;
    {
        int sent = 0;
        while (sent < msglen) {
            ssize_t n = send(fd, msg + sent, (size_t)(msglen - sent), 0);
            if (n <= 0)
                goto out;
            sent += (int)n;
        }
    }

    /* Read the length prefix, then exactly that many bytes. */
    {
        unsigned char lenb[2];
        int got = 0;
        while (got < 2) {
            ssize_t n = recv(fd, lenb + got, (size_t)(2 - got), 0);
            if (n <= 0)
                goto out;
            got += (int)n;
        }
        int want = (lenb[0] << 8) | lenb[1];
        if (want > anslen) {
            errno = EMSGSIZE;
            goto out;
        }
        got = 0;
        while (got < want) {
            ssize_t n = recv(fd, answer + got, (size_t)(want - got), 0);
            if (n <= 0)
                goto out;
            got += (int)n;
        }
        rc = want;
    }

out:;
    int saved = errno;
    close(fd);
    errno = saved;
    return rc;
}

int res_nsend(res_state statp, const unsigned char *msg, int msglen,
              unsigned char *answer, int anslen)
{
    if (!statp || !msg || !answer || msglen < NS_HFIXEDSZ) {
        errno = EINVAL;
        return -1;
    }

    struct sockaddr_storage ss;
    socklen_t slen = nameserver_addr(statp, &ss);
    if (slen == 0) {
        /* No nameserver: report it the way a refused connection reads, which
         * is what callers map onto "resolver unavailable". */
        errno = ECONNREFUSED;
        return -1;
    }

    if (statp->options & RES_USEVC)
        return send_tcp(&ss, slen, msg, msglen, answer, anslen);

    int timeout = statp->retrans > 0 ? statp->retrans : 5;
    int tries = statp->retry > 0 ? statp->retry : 2;
    for (int i = 0; i < tries; ++i) {
        int n = send_udp(&ss, slen, timeout, msg, msglen, answer, anslen);
        if (n < 0)
            return -1;
        if (n == 0)
            continue; /* timed out; try again */

        /* Truncated? Unless the caller asked to handle that itself, redo the
         * whole query over TCP. */
        if ((answer[2] & 0x02) && !(statp->options & RES_IGNTC))
            return send_tcp(&ss, slen, msg, msglen, answer, anslen);
        return n;
    }
    errno = ETIMEDOUT;
    return -1;
}

int dn_expand(const unsigned char *msg, const unsigned char *eom,
              const unsigned char *src, char *dst, int dstsiz)
{
    if (!msg || !eom || !src || !dst || dstsiz <= 0 || src < msg || src >= eom) {
        errno = EMSGSIZE;
        return -1;
    }

    const unsigned char *p = src;
    int w = 0;          /* bytes written to dst */
    int consumed = -1;  /* bytes consumed at src; fixed once we first jump */
    /* A compression pointer must point strictly backwards. Enforcing that is
     * what makes a malicious or corrupt message terminate instead of looping:
     * each jump strictly decreases the target, so there can be at most as many
     * jumps as there are bytes before src. */
    const unsigned char *limit = src;

    for (;;) {
        if (p >= eom)
            goto bad;
        unsigned char n = *p;

        if ((n & NS_CMPRSFLGS) == NS_CMPRSFLGS) {
            if (p + 1 >= eom)
                goto bad;
            const unsigned char *target = msg + (((n & ~NS_CMPRSFLGS) << 8) | p[1]);
            if (consumed < 0)
                consumed = (int)(p + 2 - src);
            if (target >= limit)
                goto bad; /* not strictly backwards */
            limit = target;
            p = target;
            continue;
        }
        if (n & NS_CMPRSFLGS)
            goto bad; /* 0x40/0x80: reserved label types */

        if (n == 0) {
            /* Root label ends the name. An empty name prints as "." */
            if (w == 0) {
                if (dstsiz < 2)
                    goto bad;
                dst[w++] = '.';
            }
            dst[w] = '\0';
            return consumed < 0 ? (int)(p + 1 - src) : consumed;
        }

        if (p + 1 + n > eom)
            goto bad;
        if (w != 0) {
            if (w + 1 >= dstsiz)
                goto bad;
            dst[w++] = '.';
        }
        if (w + n >= dstsiz || w + n > NS_MAXCDNAME)
            goto bad;
        memcpy(dst + w, p + 1, n);
        w += n;
        p += 1 + n;
    }

bad:
    errno = EMSGSIZE;
    return -1;
}
