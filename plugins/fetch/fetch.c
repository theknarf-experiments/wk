#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netdb.h>
int main(int argc, char **argv) {
    const char *host = argc > 1 ? argv[1] : "127.0.0.1";
    const char *port = argc > 2 ? argv[2] : "80";
    struct addrinfo hints, *res;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    int e = getaddrinfo(host, port, &hints, &res);
    if (e != 0) { fprintf(stderr, "getaddrinfo: %s\n", gai_strerror(e)); return 1; }
    int fd = socket(res->ai_family, res->ai_socktype, 0);
    if (fd < 0) { perror("socket"); return 1; }
    if (connect(fd, res->ai_addr, res->ai_addrlen) != 0) { perror("connect"); return 1; }
    char req[512];
    int n = snprintf(req, sizeof req,
        "GET / HTTP/1.0\r\nHost: %s\r\nConnection: close\r\n\r\n", host);
    send(fd, req, n, 0);
    char buf[4096]; ssize_t r;
    while ((r = recv(fd, buf, sizeof buf, 0)) > 0) fwrite(buf, 1, r, stdout);
    close(fd);
    freeaddrinfo(res);
    return 0;
}
