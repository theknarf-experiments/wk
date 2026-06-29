/* A tiny UDP echo demo (standard BSD sockets via wasi-libc, no wk-specific
 * code). Two modes, selected by argv[1]:
 *   server <port>            bind and echo every datagram back to its sender
 *   client <ip> <port> <msg> send <msg>, print the echoed reply, exit
 * Demonstrates node-to-node UDP over wk's userspace network fabric — wire a
 * client node and a server node onto the same Network node and they talk. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

static int run_server(int port) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) { perror("socket"); return 1; }
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = INADDR_ANY;
    addr.sin_port = htons(port);
    if (bind(fd, (struct sockaddr *)&addr, sizeof addr) != 0) { perror("bind"); return 1; }
    printf("wk udpecho: listening on :%d\n", port);
    fflush(stdout);
    for (;;) {
        char buf[2048];
        struct sockaddr_in peer;
        socklen_t plen = sizeof peer;
        ssize_t n = recvfrom(fd, buf, sizeof buf, 0, (struct sockaddr *)&peer, &plen);
        if (n < 0) continue;
        sendto(fd, buf, n, 0, (struct sockaddr *)&peer, plen);
        printf("wk udpecho: echoed %zd bytes\n", n);
        fflush(stdout);
    }
    return 0;
}

static int run_client(const char *ip, int port, const char *msg) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) { perror("socket"); return 1; }
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    inet_pton(AF_INET, ip, &addr.sin_addr);
    if (sendto(fd, msg, strlen(msg), 0, (struct sockaddr *)&addr, sizeof addr) < 0) {
        perror("sendto");
        return 1;
    }
    char buf[2048];
    ssize_t n = recv(fd, buf, sizeof buf, 0);
    if (n < 0) { perror("recv"); return 1; }
    printf("wk udpecho reply: %.*s\n", (int)n, buf);
    fflush(stdout);
    return 0;
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "server";
    if (strcmp(mode, "client") == 0) {
        const char *ip = argc > 2 ? argv[2] : "10.0.0.2";
        int port = argc > 3 ? atoi(argv[3]) : 4242;
        const char *msg = argc > 4 ? argv[4] : "ping over udp";
        return run_client(ip, port, msg);
    }
    int port = argc > 2 ? atoi(argv[2]) : 4242;
    return run_server(port);
}
