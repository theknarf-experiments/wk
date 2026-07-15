/* WASI stubs for the Unix process facilities Vim references but WASI lacks.
 *
 * The terminal itself (tcgetattr/tcsetattr/ioctl) is handled by the shared
 * ../tty-compat shim over wk's wk:tty/control capability — not here. What
 * remains is the process/job machinery: fork/exec/waitpid/pipe/select. Vim
 * compiles those paths unconditionally but they are never reachable in wk
 * (there are no subprocesses), so these only need to satisfy the linker and
 * return an error. No Vim source is modified. */

#include <errno.h>
#include <sys/types.h>
#include <unistd.h>

/* ---- process control: no subprocesses under WASI ---- */

pid_t fork(void) {
    errno = ENOSYS;
    return -1;
}

int execvp(const char *file, char *const argv[]) {
    (void)file;
    (void)argv;
    errno = ENOSYS;
    return -1;
}

int execv(const char *path, char *const argv[]) {
    (void)path;
    (void)argv;
    errno = ENOSYS;
    return -1;
}

pid_t waitpid(pid_t pid, int *status, int options) {
    (void)pid;
    (void)status;
    (void)options;
    errno = ECHILD;
    return -1;
}

pid_t wait(int *status) {
    (void)status;
    errno = ECHILD;
    return -1;
}

pid_t wait4(pid_t pid, int *status, int options, void *rusage) {
    (void)pid;
    (void)status;
    (void)options;
    (void)rusage;
    errno = ECHILD;
    return -1;
}

int pipe(int fds[2]) {
    (void)fds;
    errno = ENOSYS;
    return -1;
}

int dup2(int oldfd, int newfd) {
    (void)oldfd;
    (void)newfd;
    errno = ENOSYS;
    return -1;
}

unsigned int alarm(unsigned int seconds) {
    (void)seconds;
    return 0;
}

int setpgid(pid_t pid, pid_t pgid) {
    (void)pid;
    (void)pgid;
    return 0;
}

pid_t setsid(void) {
    return 0;
}

pid_t getpgid(pid_t pid) {
    (void)pid;
    return 1;
}

int tcsetpgrp(int fd, pid_t pgrp) {
    (void)fd;
    (void)pgrp;
    return 0;
}

pid_t tcgetpgrp(int fd) {
    (void)fd;
    return 1;
}

int killpg(int pgrp, int sig) {
    (void)pgrp;
    (void)sig;
    errno = ESRCH;
    return -1;
}

int kill(pid_t pid, int sig) {
    (void)pid;
    (void)sig;
    errno = ESRCH;
    return -1;
}

/* ---- misc POSIX facilities WASI omits ---- */

/* Single-user sandbox: report root so Vim never downgrades behaviour on a
 * "running as another user" check. */
uid_t getuid(void) { return 0; }
gid_t getgid(void) { return 0; }

/* No fd table to duplicate, no permission bits, no disk to flush. */
int dup(int oldfd) {
    (void)oldfd;
    errno = ENOSYS;
    return -1;
}

mode_t umask(mode_t mask) {
    (void)mask;
    return 0;
}

/* No header declares sync() here, so its callers assume the K&R `int sync()`;
 * match that so the wasm call signatures agree (a void return would make
 * wasm-ld insert a trapping trampoline). Nothing to flush under WASI. */
int sync(void) { return 0; }
