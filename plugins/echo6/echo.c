/* A tiny IPv6 (AF_INET6) demo over wk's userspace fabric (standard BSD sockets,
 * no wk-specific code). Two modes:
 *   server <port>       bind [::]/in6addr_any, accept conns, send a banner
 *   client <ip6> <port> connect to the v6 literal (e.g. fd00::3), print the reply
 * Wire a client and server node onto the same Network node and they talk over
 * IPv6. Fabric v6 addresses are fd00::<host-octet> (mirroring 10.0.0.<octet>). */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

static int run_server(int port) {
    int sfd = socket(AF_INET6, SOCK_STREAM, 0);
    if (sfd < 0) { perror("socket"); return 1; }
    struct sockaddr_in6 addr;
    memset(&addr, 0, sizeof addr);
    addr.sin6_family = AF_INET6;
    addr.sin6_addr = in6addr_any;
    addr.sin6_port = htons(port);
    if (bind(sfd, (struct sockaddr *)&addr, sizeof addr) != 0) { perror("bind"); return 1; }
    if (listen(sfd, 1) != 0) { perror("listen"); return 1; }
    printf("wk echo6: listening on [::]:%d\n", port);
    fflush(stdout);
    for (;;) {
        int c = accept(sfd, 0, 0);
        if (c < 0) continue;
        char buf[256];
        (void)recv(c, buf, sizeof buf, 0);
        const char *resp = "hello over ipv6\n";
        send(c, resp, strlen(resp), 0);
        close(c);
        printf("wk echo6: served a request\n");
        fflush(stdout);
    }
    return 0;
}

static int run_client(const char *ip, int port) {
    int fd = socket(AF_INET6, SOCK_STREAM, 0);
    if (fd < 0) { perror("socket"); return 1; }
    struct sockaddr_in6 addr;
    memset(&addr, 0, sizeof addr);
    addr.sin6_family = AF_INET6;
    addr.sin6_port = htons(port);
    if (inet_pton(AF_INET6, ip, &addr.sin6_addr) != 1) { perror("inet_pton"); return 1; }
    if (connect(fd, (struct sockaddr *)&addr, sizeof addr) != 0) { perror("connect"); return 1; }
    const char *greeting = "ping\n";
    send(fd, greeting, strlen(greeting), 0);
    char buf[256];
    ssize_t n = recv(fd, buf, sizeof buf, 0);
    if (n < 0) { perror("recv"); return 1; }
    printf("wk echo6 reply: %.*s", (int)n, buf);
    fflush(stdout);
    return 0;
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "server";
    if (strcmp(mode, "client") == 0) {
        const char *ip = argc > 2 ? argv[2] : "fd00::3";
        int port = argc > 3 ? atoi(argv[3]) : 80;
        return run_client(ip, port);
    }
    int port = argc > 2 ? atoi(argv[2]) : 80;
    return run_server(port);
}
