/* httpfs: a read-only FUSE filesystem backed by an HTTP file server, reached
 * over BSD sockets — which in wk ride the network fabric. Point it at a
 * server node by fabric name (`httpfs http://filesrv:8080`) and that
 * server's files appear wherever this node is mounted: a network filesystem
 * whose network is the canvas.
 *
 * Server conventions (the least a static file server provides):
 *   - `GET  /path`          file bytes; `Range: bytes=a-b` honored (206) or
 *                           ignored (200 + full body, sliced client-side)
 *   - `HEAD /path`          file existence + Content-Length
 *   - `GET  /path/`         autoindex as text/plain, one entry per line,
 *                           directories with a trailing slash
 * Each request is its own connection (Connection: close); the reply is read
 * to EOF. No caching: every filesystem operation is one round trip, which
 * over the in-process fabric costs a hub tick, not a network.
 */

#define FUSE_USE_VERSION 31

#include <errno.h>
#include <fcntl.h>
#include <fuse.h>
#include <netdb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static struct {
    char host[256];
    char port[16];
    char prefix[512]; /* path prefix from the base URL, no trailing slash */
} g;

/* One HTTP exchange. Returns the status code (or -errno), with the response
 * body malloc'd into *out_body / *out_len when non-NULL is passed. */
static int http_request(const char *method, const char *path,
                        const char *extra_headers, char **out_body,
                        size_t *out_len) {
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(g.host, g.port, &hints, &res) != 0 || !res)
        return -EIO;
    int fd = socket(res->ai_family, res->ai_socktype, 0);
    if (fd < 0) {
        freeaddrinfo(res);
        return -EIO;
    }
    if (connect(fd, res->ai_addr, res->ai_addrlen) != 0) {
        freeaddrinfo(res);
        close(fd);
        return -EIO;
    }
    freeaddrinfo(res);

    char req[1024];
    int n = snprintf(req, sizeof(req),
                     "%s %s%s HTTP/1.1\r\n"
                     "Host: %s\r\n"
                     "Connection: close\r\n"
                     "%s"
                     "\r\n",
                     method, g.prefix, path, g.host,
                     extra_headers ? extra_headers : "");
    if (n < 0 || (size_t)n >= sizeof(req) || write(fd, req, n) != n) {
        close(fd);
        return -EIO;
    }

    /* Read the whole reply (headers + body) to EOF. */
    size_t cap = 8192, len = 0;
    char *buf = malloc(cap);
    for (;;) {
        if (len == cap) {
            cap *= 2;
            buf = realloc(buf, cap);
        }
        ssize_t r = read(fd, buf + len, cap - len);
        if (r < 0) {
            close(fd);
            free(buf);
            return -EIO;
        }
        if (r == 0)
            break;
        len += (size_t)r;
    }
    close(fd);

    int status = 0;
    if (len < 12 || sscanf(buf, "HTTP/%*d.%*d %d", &status) != 1) {
        free(buf);
        return -EIO;
    }
    char *body = memmem(buf, len, "\r\n\r\n", 4);
    if (out_body && body) {
        size_t blen = len - (size_t)(body + 4 - buf);
        *out_body = malloc(blen ? blen : 1);
        memcpy(*out_body, body + 4, blen);
        *out_len = blen;
    } else if (out_body) {
        *out_body = malloc(1);
        *out_len = 0;
    }
    free(buf);
    return status;
}

/* Content-Length of a file, via HEAD. -ENOENT when it isn't a file there. */
static long long head_size(const char *path) {
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(g.host, g.port, &hints, &res) != 0 || !res)
        return -EIO;
    int fd = socket(res->ai_family, res->ai_socktype, 0);
    if (fd < 0 || connect(fd, res->ai_addr, res->ai_addrlen) != 0) {
        freeaddrinfo(res);
        if (fd >= 0)
            close(fd);
        return -EIO;
    }
    freeaddrinfo(res);
    char req[1024];
    int n = snprintf(req, sizeof(req),
                     "HEAD %s%s HTTP/1.1\r\nHost: %s\r\nConnection: close\r\n\r\n",
                     g.prefix, path, g.host);
    if (write(fd, req, n) != n) {
        close(fd);
        return -EIO;
    }
    char buf[4096];
    size_t len = 0;
    for (;;) {
        ssize_t r = read(fd, buf + len, sizeof(buf) - 1 - len);
        if (r <= 0)
            break;
        len += (size_t)r;
        if (len >= sizeof(buf) - 1)
            break;
    }
    close(fd);
    buf[len] = '\0';
    int status = 0;
    if (sscanf(buf, "HTTP/%*d.%*d %d", &status) != 1)
        return -EIO;
    if (status != 200)
        return -ENOENT;
    long long size = 0;
    for (char *line = buf; line; line = strstr(line, "\r\n")) {
        while (*line == '\r' || *line == '\n')
            line++;
        if (strncasecmp(line, "Content-Length:", 15) == 0) {
            size = atoll(line + 15);
            break;
        }
    }
    return size;
}

/* GET path + "/" — a directory listing, or NULL. */
static char *get_listing(const char *path) {
    char dir[768];
    snprintf(dir, sizeof(dir), "%s%s", path,
             path[strlen(path) - 1] == '/' ? "" : "/");
    char *body = NULL;
    size_t blen = 0;
    int status = http_request("GET", dir, NULL, &body, &blen);
    if (status != 200) {
        free(body);
        return NULL;
    }
    char *text = realloc(body, blen + 1);
    text[blen] = '\0';
    return text;
}

/* ---- FUSE callbacks ---- */

static int httpfs_getattr(const char *path, struct stat *st,
                          struct fuse_file_info *fi) {
    (void)fi;
    memset(st, 0, sizeof(*st));
    if (strcmp(path, "/") == 0) {
        st->st_mode = S_IFDIR | 0555;
        return 0;
    }
    long long size = head_size(path);
    if (size >= 0) {
        st->st_mode = S_IFREG | 0444;
        st->st_size = (off_t)size;
        return 0;
    }
    char *listing = get_listing(path);
    if (listing) {
        free(listing);
        st->st_mode = S_IFDIR | 0555;
        return 0;
    }
    return -ENOENT;
}

static int httpfs_readdir(const char *path, void *buf, fuse_fill_dir_t filler,
                          off_t offset, struct fuse_file_info *fi,
                          enum fuse_readdir_flags flags) {
    (void)offset;
    (void)fi;
    (void)flags;
    char *listing = get_listing(path);
    if (!listing)
        return -ENOENT;
    filler(buf, ".", NULL, 0, 0);
    filler(buf, "..", NULL, 0, 0);
    for (char *line = strtok(listing, "\n"); line; line = strtok(NULL, "\n")) {
        size_t n = strlen(line);
        while (n > 0 && (line[n - 1] == '\r'))
            line[--n] = '\0';
        if (n == 0)
            continue;
        struct stat st = {0};
        if (line[n - 1] == '/') {
            line[n - 1] = '\0';
            st.st_mode = S_IFDIR | 0555;
        } else {
            st.st_mode = S_IFREG | 0444;
        }
        filler(buf, line, &st, 0, 0);
    }
    free(listing);
    return 0;
}

static int httpfs_open(const char *path, struct fuse_file_info *fi) {
    if ((fi->flags & O_ACCMODE) != O_RDONLY)
        return -EROFS;
    return head_size(path) >= 0 ? 0 : -ENOENT;
}

static int httpfs_read(const char *path, char *buf, size_t size, off_t offset,
                       struct fuse_file_info *fi) {
    (void)fi;
    char range[80];
    snprintf(range, sizeof(range), "Range: bytes=%lld-%lld\r\n",
             (long long)offset, (long long)offset + (long long)size - 1);
    char *body = NULL;
    size_t blen = 0;
    int status = http_request("GET", path, range, &body, &blen);
    if (status == 206) {
        /* The server honored the range: the body is our slice. */
        if (blen > size)
            blen = size;
        memcpy(buf, body, blen);
        free(body);
        return (int)blen;
    }
    if (status == 200) {
        /* Full body: slice it client-side. */
        if ((size_t)offset >= blen) {
            free(body);
            return 0;
        }
        size_t n = blen - (size_t)offset;
        if (n > size)
            n = size;
        memcpy(buf, body + offset, n);
        free(body);
        return (int)n;
    }
    free(body);
    return status == 404 ? -ENOENT : -EIO;
}

static const struct fuse_operations httpfs_oper = {
    .getattr = httpfs_getattr,
    .readdir = httpfs_readdir,
    .open = httpfs_open,
    .read = httpfs_read,
};

int main(int argc, char *argv[]) {
    const char *url = argc > 1 ? argv[1] : NULL;
    if (!url || strncmp(url, "http://", 7) != 0) {
        fprintf(stderr, "usage: httpfs http://<host>[:port][/prefix]\n");
        return 1;
    }
    const char *host = url + 7;
    const char *slash = strchr(host, '/');
    const char *colon = strchr(host, ':');
    if (colon && (!slash || colon < slash)) {
        snprintf(g.host, sizeof(g.host), "%.*s", (int)(colon - host), host);
        snprintf(g.port, sizeof(g.port), "%.*s",
                 slash ? (int)(slash - colon - 1) : (int)strlen(colon + 1),
                 colon + 1);
    } else {
        snprintf(g.host, sizeof(g.host), "%.*s",
                 slash ? (int)(slash - host) : (int)strlen(host), host);
        snprintf(g.port, sizeof(g.port), "80");
    }
    if (slash && slash[1] != '\0') {
        snprintf(g.prefix, sizeof(g.prefix), "%s", slash);
        size_t n = strlen(g.prefix);
        if (n > 0 && g.prefix[n - 1] == '/')
            g.prefix[n - 1] = '\0';
    }
    fprintf(stderr, "httpfs: serving %s:%s%s\n", g.host, g.port, g.prefix);
    return fuse_main(argc, argv, &httpfs_oper, NULL);
}
