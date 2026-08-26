/* dnsstub — a deliberately minimal authoritative DNS server, as a wk node.
 *
 * Exists so that DNS can be tested on the fabric HERMETICALLY. wk's own name
 * service (wasi:sockets ip-name-lookup, which getaddrinfo/QHostInfo use)
 * answers node names but only ever returns ADDRESSES; there is nothing on a
 * virtual network that answers MX, TXT or SRV. Pointing a test at a real
 * resolver would make it depend on the internet and on records somebody else
 * controls, so instead: wire this node onto a Network, point a client's
 * nameserver at it, and every answer is one this file wrote.
 *
 * It is a STUB, and the name is the honest description. It answers from a
 * fixed table, ignores the query class, never compresses names, has no zone
 * transfer, no recursion, no EDNS0, no truncation and no caching. What it does
 * do correctly is the wire format: a real DNS client must be able to parse the
 * response, which is the whole point — plugins/resolv-compat sends the query
 * and Qt's qdnslookup_unix.cpp parses the reply, and neither is being tested
 * if the server is not honest about the encoding.
 *
 * Records served (all for `wk.test`, class IN):
 *   A     10.0.0.42
 *   MX    10 mail.wk.test
 *   TXT   "wk dnsstub"
 *   NS    ns.wk.test
 * Anything else gets NXDOMAIN, which is itself worth being able to test.
 *
 * Usage: dnsstub [port]   (default 53)
 */
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define T_A 1
#define T_NS 2
#define T_TXT 16
#define T_MX 15

/* The one name this stub is authoritative for, as wire labels. */
static const unsigned char ZONE[] = { 2, 'w', 'k', 4, 't', 'e', 's', 't', 0 };

/* Decode a QNAME at buf[off] into `out` (dotted, lowercase). Returns the new
 * offset, or -1. Compression pointers are rejected: a QUESTION section never
 * contains one, and accepting them here would only add a way to be wrong. */
static int read_qname(const unsigned char *buf, int len, int off, char *out, int outlen)
{
    int w = 0;
    while (off < len) {
        unsigned char n = buf[off];
        if (n == 0)
            return (out[w] = '\0'), off + 1;
        if (n & 0xc0)
            return -1;
        if (off + 1 + n > len || w + n + 2 > outlen)
            return -1;
        if (w)
            out[w++] = '.';
        for (int i = 0; i < n; ++i) {
            char c = (char)buf[off + 1 + i];
            out[w++] = (c >= 'A' && c <= 'Z') ? (char)(c + 32) : c;
        }
        off += 1 + n;
    }
    return -1;
}

static int put_u16(unsigned char *p, unsigned v)
{
    p[0] = (unsigned char)(v >> 8);
    p[1] = (unsigned char)(v & 0xff);
    return 2;
}

/* Resource-record header: NAME (the zone, uncompressed) TYPE CLASS TTL RDLEN.
 * Returns bytes written; the caller fills RDATA and back-patches RDLEN. */
static int put_rr_head(unsigned char *p, int type)
{
    int w = 0;
    memcpy(p + w, ZONE, sizeof ZONE);
    w += (int)sizeof ZONE;
    w += put_u16(p + w, (unsigned)type);
    w += put_u16(p + w, 1);      /* class IN */
    w += put_u16(p + w, 0);      /* TTL high */
    w += put_u16(p + w, 60);     /* TTL low: 60s */
    w += put_u16(p + w, 0);      /* RDLEN, patched by the caller */
    return w;
}

int main(int argc, char **argv)
{
    int port = argc > 1 ? atoi(argv[1]) : 53;

    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) {
        perror("socket");
        return 1;
    }
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = INADDR_ANY;
    addr.sin_port = htons((unsigned short)port);
    if (bind(fd, (struct sockaddr *)&addr, sizeof addr) != 0) {
        perror("bind");
        return 1;
    }
    printf("wk dnsstub: authoritative for wk.test on :%d\n", port);
    fflush(stdout);

    for (;;) {
        unsigned char q[1500], a[1500];
        struct sockaddr_in peer;
        socklen_t plen = sizeof peer;
        ssize_t n = recvfrom(fd, q, sizeof q, 0, (struct sockaddr *)&peer, &plen);
        if (n < 12)
            continue;

        char name[256];
        int off = read_qname(q, (int)n, 12, name, sizeof name);
        if (off < 0 || off + 4 > (int)n)
            continue;
        int qtype = (q[off] << 8) | q[off + 1];
        int qlen = off + 4; /* end of the question section */

        /* Header: copy the id, then QR=1 AA=1 and RD echoed back. */
        memcpy(a, q, (size_t)qlen);
        a[2] = (unsigned char)(0x84 | (q[2] & 0x01)); /* QR | AA | RD */
        a[3] = 0;
        int ancount = 0;
        int w = qlen;

        if (strcmp(name, "wk.test") != 0) {
            a[3] = 3; /* NXDOMAIN */
        } else if (qtype == T_A) {
            int h = put_rr_head(a + w, T_A);
            put_u16(a + w + h - 2, 4);
            w += h;
            a[w++] = 10; a[w++] = 0; a[w++] = 0; a[w++] = 42;
            ancount = 1;
        } else if (qtype == T_MX) {
            static const unsigned char mail[] = { 4, 'm', 'a', 'i', 'l', 2, 'w', 'k',
                                                  4, 't', 'e', 's', 't', 0 };
            int h = put_rr_head(a + w, T_MX);
            put_u16(a + w + h - 2, (unsigned)(2 + sizeof mail));
            w += h;
            w += put_u16(a + w, 10); /* preference */
            memcpy(a + w, mail, sizeof mail);
            w += (int)sizeof mail;
            ancount = 1;
        } else if (qtype == T_TXT) {
            static const char txt[] = "wk dnsstub";
            int h = put_rr_head(a + w, T_TXT);
            put_u16(a + w + h - 2, (unsigned)(1 + sizeof txt - 1));
            w += h;
            a[w++] = (unsigned char)(sizeof txt - 1); /* one character-string */
            memcpy(a + w, txt, sizeof txt - 1);
            w += (int)sizeof txt - 1;
            ancount = 1;
        } else if (qtype == T_NS) {
            static const unsigned char ns[] = { 2, 'n', 's', 2, 'w', 'k',
                                                4, 't', 'e', 's', 't', 0 };
            int h = put_rr_head(a + w, T_NS);
            put_u16(a + w + h - 2, (unsigned)sizeof ns);
            w += h;
            memcpy(a + w, ns, sizeof ns);
            w += (int)sizeof ns;
            ancount = 1;
        }
        /* else: a type we do not serve -> NOERROR with no answers, which is
         * what a real authoritative server returns for a name it has but a
         * type it does not. */

        put_u16(a + 6, (unsigned)ancount);
        sendto(fd, a, (size_t)w, 0, (struct sockaddr *)&peer, plen);
        printf("wk dnsstub: %s type=%d -> %d answer(s), rcode=%d\n",
               name, qtype, ancount, a[3] & 0x0f);
        fflush(stdout);
    }
    return 0;
}
