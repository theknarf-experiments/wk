/* POSIX odds and ends pdftex + kpathsea reference but wasi-libc (wasi-sdk
 * 34-rc.2, wasm32-wasip2) does not provide. Everything here is either a clean
 * failure (no processes on WASI: system/popen/fork), an inert answer (umask,
 * alarm), or a real implementation in terms of what WASI does have (mkstemp).
 *
 * Symbols were probed against exactly this SDK's libc before being defined
 * here — a definition that collides with a future libc's own would be a
 * duplicate-symbol error at link, which is the loud failure we want.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

/* wk:exec cannot set a child's initial cwd — wasi-libc starts at "/" until
 * something chdir()s. The shell passes the intended cwd as __WK_EXEC_CWD and
 * children chdir into it before main (the same constructor bash + coreutils
 * link from plugins/exec-compat/chdir_shim.c). It lives in this object, which
 * the linker always pulls (pdftex needs the other symbols here), so archive
 * member laziness cannot drop it. */
__attribute__((constructor)) static void wk_exec_chdir(void) {
    const char *d = getenv("__WK_EXEC_CWD");
    if (d && *d)
        (void)chdir(d);
}

/* No shell and no processes on WASI. TeX probes `system(NULL)` to ask whether
 * a shell exists: answer no, so \write18 degrades the way shell-escape being
 * disabled does, rather than erroring mid-run. */
int system(const char *cmd) {
    if (cmd == NULL)
        return 0; /* "no shell available" */
    errno = ENOSYS;
    return -1;
}

FILE *popen(const char *cmd, const char *mode) {
    (void)cmd;
    (void)mode;
    errno = ENOSYS;
    return NULL;
}

int pclose(FILE *f) {
    (void)f;
    errno = ENOSYS;
    return -1;
}

int fork(void) {
    errno = ENOSYS;
    return -1;
}

int waitpid(int pid, int *status, int options) {
    (void)pid;
    (void)status;
    (void)options;
    errno = ENOSYS;
    return -1;
}

int execv(const char *path, char *const argv[]) {
    (void)path;
    (void)argv;
    errno = ENOSYS;
    return -1;
}

int execvp(const char *file, char *const argv[]) {
    (void)file;
    (void)argv;
    errno = ENOSYS;
    return -1;
}

int wait(int *status) {
    (void)status;
    errno = ENOSYS;
    return -1;
}

int kill(int pid, int sig) {
    (void)pid;
    (void)sig;
    errno = ENOSYS;
    return -1;
}

/* Nothing to mask in a single-user sandbox. */
int umask(int mask) {
    (void)mask;
    return 0;
}

unsigned alarm(unsigned seconds) {
    (void)seconds;
    return 0;
}

/* User-database lookups: nobody home. The stub <pwd.h> in compat/include
 * declares these; a NULL answer makes ~user expansion fall back cleanly. */
struct wk_passwd; /* layout in compat/include/pwd.h; opaque here */
struct wk_passwd *getpwnam(const char *name) {
    (void)name;
    return 0;
}
struct wk_passwd *getpwuid(unsigned uid) {
    (void)uid;
    return 0;
}

/* uid queries: one anonymous user. kpathsea only asks on paths that configure
 * usually guards behind HAVE_PWD_H, but web2c's own uses aren't all guarded. */
int getuid(void) { return 0; }
int geteuid(void) { return 0; }
int getgid(void) { return 0; }
int getegid(void) { return 0; }
int setuid(int uid) { (void)uid; return 0; }
int setgid(int gid) { (void)gid; return 0; }

/* A real mkstemp: O_EXCL retries over a pseudo-random suffix. WASI has no
 * getpid entropy worth using; clock ticks are plenty for a tmpname. */
int mkstemp(char *template_) {
    size_t len = strlen(template_);
    if (len < 6 || strcmp(template_ + len - 6, "XXXXXX") != 0) {
        errno = EINVAL;
        return -1;
    }
    char *sfx = template_ + len - 6;
    static const char alphabet[] =
        "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    unsigned long seed = 0;
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) == 0)
        seed = (unsigned long)ts.tv_nsec ^ (unsigned long)ts.tv_sec;
    for (int attempt = 0; attempt < 100; attempt++) {
        unsigned long r = seed + (unsigned long)attempt * 7919u;
        for (int i = 0; i < 6; i++) {
            sfx[i] = alphabet[r % (sizeof(alphabet) - 1)];
            r /= sizeof(alphabet) - 1;
            r ^= r << 13;
        }
        int fd = open(template_, O_RDWR | O_CREAT | O_EXCL, 0600);
        if (fd >= 0)
            return fd;
        if (errno != EEXIST)
            return -1;
        seed = seed * 6364136223846793005ull + 1442695040888963407ull;
    }
    errno = EEXIST;
    return -1;
}

char *mkdtemp(char *template_) {
    size_t len = strlen(template_);
    if (len < 6 || strcmp(template_ + len - 6, "XXXXXX") != 0) {
        errno = EINVAL;
        return NULL;
    }
    /* Reuse mkstemp's name generation by probing with mkdir directly. */
    char *sfx = template_ + len - 6;
    static const char alphabet[] =
        "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    unsigned long seed = 0;
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) == 0)
        seed = (unsigned long)ts.tv_nsec ^ (unsigned long)ts.tv_sec;
    for (int attempt = 0; attempt < 100; attempt++) {
        unsigned long r = seed + (unsigned long)attempt * 7919u;
        for (int i = 0; i < 6; i++) {
            sfx[i] = alphabet[r % (sizeof(alphabet) - 1)];
            r /= sizeof(alphabet) - 1;
            r ^= r << 13;
        }
        if (mkdir(template_, 0700) == 0)
            return template_;
        if (errno != EEXIST)
            return NULL;
        seed = seed * 6364136223846793005ull + 1442695040888963407ull;
    }
    errno = EEXIST;
    return NULL;
}

FILE *tmpfile(void) {
    const char *dir = getenv("TMPDIR");
    if (dir == NULL || *dir == '\0')
        dir = "/tmp";
    char path[256];
    snprintf(path, sizeof(path), "%s/tmpXXXXXX", dir);
    int fd = mkstemp(path);
    if (fd < 0)
        return NULL;
    /* Unlink-while-open works on wk's vfs the way it does on POSIX. */
    unlink(path);
    return fdopen(fd, "w+b");
}
