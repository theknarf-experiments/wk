/* A tiny TCP server (standard BSD sockets via wasi-libc, no wk-specific code):
 * bind/listen/accept, serve a fixed HTTP/1.0 banner to each connection. Used to
 * demonstrate node-to-node networking over wk's userspace fabric — wire a client
 * node (e.g. fetch) to this one and it gets the banner. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

int main(int argc, char **argv) {
    int port = argc > 1 ? atoi(argv[1]) : 80;
    int sfd = socket(AF_INET, SOCK_STREAM, 0);
    if (sfd < 0) { perror("socket"); return 1; }
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = INADDR_ANY;
    addr.sin_port = htons(port);
    if (bind(sfd, (struct sockaddr *)&addr, sizeof addr) != 0) { perror("bind"); return 1; }
    if (listen(sfd, 1) != 0) { perror("listen"); return 1; }
    printf("wk netserve: listening on :%d\n", port);
    fflush(stdout);
    for (;;) {
        int c = accept(sfd, 0, 0);
        if (c < 0) continue;
        char buf[1024];
        (void)recv(c, buf, sizeof buf, 0);
        const char *resp =
            "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n"
            "Connection: close\r\n\r\nhello from a wk node\n";
        send(c, resp, strlen(resp), 0);
        close(c);
        printf("wk netserve: served a request\n");
        fflush(stdout);
    }
    return 0;
}
