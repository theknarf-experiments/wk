/* Nothing here knows about wk: it is the pipe(2) any C program writes. */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#define SAY(...) do { printf(__VA_ARGS__); fflush(stdout); } while (0)

int main(void) {
    int fd[2];
    if (pipe(fd) != 0) { SAY("pipe: %s\n", strerror(errno)); return 1; }
    SAY("pipe() -> fds %d,%d\n", fd[0], fd[1]);

    ssize_t n = write(fd[1], "through libc write\n", 19);
    SAY("write = %zd\n", n);

    char buf[64] = {0};
    n = read(fd[0], buf, sizeof buf - 1);
    SAY("read = %zd: %s", n, buf);

    struct stat st;
    fstat(fd[0], &st);
    SAY("S_ISFIFO = %d (a pipe, not a file)\n", S_ISFIFO(st.st_mode) ? 1 : 0);

    /* dup the read end: libc's dup, on our descriptor */
    int d = dup(fd[0]);
    SAY("dup(read end) = %d\n", d);

    /* closing every writer must give the reader EOF */
    write(fd[1], "last", 4);
    close(fd[1]);
    n = read(fd[0], buf, sizeof buf - 1);
    SAY("after close, read = %zd (%.4s)\n", n, buf);
    n = read(fd[0], buf, sizeof buf - 1);
    SAY("at EOF, read = %zd (0 means EOF)\n", n);

    close(d);
    close(fd[0]);
    SAY("selftest done\n");
    return 0;
}
