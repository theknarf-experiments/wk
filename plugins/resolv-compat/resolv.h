/* resolv.h — a libresolv for wasm32-wasip2, over ordinary BSD sockets.
 *
 * The networking sibling of ../tty-compat (termios), ../gfx-compat (wasi-gfx),
 * ../pipe-compat (pipe()) and ../libfuse-compat (FUSE): wasi-libc ships
 * <arpa/nameser.h> — every DNS constant, type code and rcode enum — but no
 * resolver at all, so there is no <resolv.h>, no `HEADER`, no `res_state` and
 * none of the res_n* entry points. Anything that speaks DNS *records* rather
 * than just names therefore does not build.
 *
 * WHY THIS EXISTS. wasi-libc's getaddrinfo() works fine on wk — it goes
 * through wasi:sockets/ip-name-lookup, which the fabric answers, and that is
 * what plugins/fetch and plugins/curl use. But it only ever returns
 * ADDRESSES. Qt's QDnsLookup is the API for MX, SRV, TXT, NS, PTR, CNAME and
 * SOA records, and Qt implements it in qdnslookup_unix.cpp on top of libresolv
 * — a file that is otherwise entirely portable, because Qt parses the DNS
 * response itself and only borrows the transport (res_nmkquery/res_nsend) and
 * name decompression (dn_expand). Providing those four functions makes that
 * upstream file compile and work unmodified, which is far better than forking
 * Qt's record parser.
 *
 * WHAT IT TALKS TO. res_nsend() sends a real DNS query over a real UDP socket,
 * so it reaches whatever the node's network reaches: a nameserver node on the
 * same wk Network, or — for a node wired to a Gateway — a real resolver on the
 * host network. It does NOT resolve sibling node names; those live in the
 * fabric's own name service, which getaddrinfo()/QHostInfo already reach.
 * Truncated (TC) answers retry over TCP, as RFC 1035 requires.
 *
 * WHAT IT IS NOT. There is no /etc/resolv.conf parsing beyond a plain
 * `nameserver <ip>` line, no search-domain handling, no ndots, no DNSSEC
 * validation (the AD bit is passed through, never checked), and no caching.
 * A caller that sets a nameserver explicitly — which is what QDnsLookup does
 * with QDnsLookup::setNameserver() — needs none of it.
 */
#ifndef WK_RESOLV_COMPAT_H
#define WK_RESOLV_COMPAT_H

#include <netinet/in.h>
#include <sys/types.h>
#include <arpa/nameser.h>

#ifdef __cplusplus
extern "C" {
#endif

/* NOTE: the BIND `HEADER` struct is NOT defined here. wasi-libc's
 * <arpa/nameser.h> already provides it (and every DNS constant, type code and
 * rcode enum) — only the resolver itself is missing. Defining it again is a
 * typedef-redefinition error, so callers that cast a reply buffer to HEADER,
 * as Qt does, just include <arpa/nameser.h> as usual. */

#ifndef MAXNS
#  define MAXNS 3
#endif

/* Resolver options. Only the ones a caller might set are honoured; the rest
 * exist so that `state->options |= RES_FOO` compiles, which is how these are
 * used in practice. RES_IGNTC is honoured (it suppresses the TCP retry) and
 * RES_USEVC forces TCP; the others are accepted and ignored. */
#define RES_INIT        0x00000001
#define RES_DEBUG       0x00000002
#define RES_USEVC       0x00000008
#define RES_IGNTC       0x00000020
#define RES_RECURSE     0x00000040
#define RES_DEFNAMES    0x00000080
#define RES_DNSRCH      0x00000200
#define RES_TRUSTAD     0x00008000
#define RES_DEFAULT     (RES_RECURSE | RES_DEFNAMES | RES_DNSRCH)

/* glibc's __res_state, cut down to the members a caller actually touches.
 * The `_u._ext` shape matters: Qt detects IPv6-nameserver support with
 * `sizeof(state._u._ext.nsaddrs) != 0`, so declaring it is what enables
 * setting an IPv6 nameserver at all. */
#ifndef MAXDNSRCH
#  define MAXDNSRCH 6
#endif

struct __res_state {
    int retrans;                        /* per-query timeout, seconds */
    int retry;                          /* attempts per nameserver */
    unsigned long options;
    int nscount;                        /* count of IPv4 nameservers */
    struct sockaddr_in nsaddr_list[MAXNS];
    unsigned short id;
    /* The local domain and search list, from resolv.conf's `domain`/`search`.
     * Present because QHostInfo::localDomainName() reads them directly off
     * the global _res below — QDnsLookup itself never touches them. Both are
     * empty when the node has no resolv.conf, which is the honest answer:
     * a sandbox with no resolver config has no local domain. */
    char defdname[256];
    char *dnsrch[MAXDNSRCH + 1];
    unsigned ndots : 4;
    union {
        struct {
            uint16_t nsmap[MAXNS + 1];
            int nssocks[MAXNS];
            uint16_t nscount6;
            uint16_t nsinit;
            struct sockaddr_in6 *nsaddrs[MAXNS];
        } _ext;
    } _u;
};
typedef struct __res_state *res_state;

/* Initialise `statp`: zeroes it, applies RES_DEFAULT and the default
 * timeouts, and reads a single `nameserver <ip>` line from /etc/resolv.conf
 * if the node's filesystem has one. Returns 0, or -1 with errno set.
 *
 * Unlike glibc there is no system-wide resolver state to inherit, so a node
 * with no /etc/resolv.conf simply starts with no nameserver — res_nsend()
 * then fails with ECONNREFUSED unless the caller sets one. */
int res_ninit(res_state statp);

/* The legacy global resolver state, and res_init() as res_ninit(&_res).
 *
 * Provided because the BIND-era global API has not gone away: Qt's
 * qhostinfo_unix.cpp reads `_res.options`, calls res_init() and then reads
 * `_res.defdname`/`_res.dnsrch` to answer QHostInfo::localDomainName(). It is
 * not thread-safe by construction, which costs nothing here — a wasip2
 * component has no threads. */
extern struct __res_state _res;
int res_init(void);

/* Release anything res_ninit()/nameserver-setting allocated — specifically
 * the _u._ext.nsaddrs[] entries, which callers are expected to calloc() into
 * (glibc documents res_close() as owning them, and Qt relies on it). */
void res_nclose(res_state statp);

/* Build a standard query for `dname`/`class`/`type` into `buf`. `op` must be
 * QUERY; `data`, `datalen` and `newrr_in` are accepted for signature
 * compatibility and must be NULL/0. Returns the query length, or -1 with
 * errno set (ENOSPC if `buflen` is too small, EINVAL for a malformed name). */
int res_nmkquery(res_state statp, int op, const char *dname, int class_, int type,
                 const unsigned char *data, int datalen, const unsigned char *newrr_in,
                 unsigned char *buf, int buflen);

/* Send `msg` to the configured nameserver and write the reply into `answer`.
 * Returns the reply length, or -1 with errno set: ECONNREFUSED if no
 * nameserver is configured or the send failed, ETIMEDOUT if no reply arrived
 * within retrans*retry seconds, EMSGSIZE if the reply does not fit.
 *
 * UDP first; if the reply has TC set and RES_IGNTC is clear, the query is
 * retried over TCP. A caller that wants to handle truncation itself sets
 * RES_IGNTC and inspects the header, which is what Qt does. */
int res_nsend(res_state statp, const unsigned char *msg, int msglen,
              unsigned char *answer, int anslen);

/* Expand a possibly-compressed domain name at `src` into `dst` as a
 * printable dotted string. `msg`/`eom` bound the message for following
 * compression pointers. Returns the number of bytes consumed AT `src`
 * (a pointer counts as 2), or -1 on a malformed name, a pointer that does not
 * point strictly backwards (the loop guard), or an oversized result. */
int dn_expand(const unsigned char *msg, const unsigned char *eom,
              const unsigned char *src, char *dst, int dstsiz);

#ifdef __cplusplus
}
#endif

#endif /* WK_RESOLV_COMPAT_H */
