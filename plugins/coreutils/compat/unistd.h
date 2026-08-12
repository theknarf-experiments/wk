/* <unistd.h> wrapper for wasm32-wasi.
 *
 * WASI's unistd.h omits the uid/gid family entirely (no users in the
 * sandbox), but coreutils calls geteuid()/getuid() in a dozen places. Chain to
 * the real header, then add the declarations compat.c implements — so upstream
 * source compiles unedited, and the include order gnulib insists on
 * (config.h first) is untouched.
 */
#ifndef _WK_COMPAT_UNISTD_H
#define _WK_COMPAT_UNISTD_H

#include_next <unistd.h>

#include <sys/types.h>

uid_t getuid(void);
uid_t geteuid(void);
gid_t getgid(void);
gid_t getegid(void);
int getgroups(int size, gid_t list[]);
int initgroups(const char *user, gid_t group);
int setuid(uid_t uid);
int seteuid(uid_t uid);
int setgid(gid_t gid);
int setegid(gid_t gid);

/* Ownership: WASI's filesystem has no uid/gid, so these are declared for the
 * chown/chgrp/cp code paths and fail with ENOSYS (nothing to change). */
int chown(const char *path, uid_t owner, gid_t group);
int fchown(int fd, uid_t owner, gid_t group);
int lchown(const char *path, uid_t owner, gid_t group);
int fchownat(int fd, const char *path, uid_t owner, gid_t group, int flag);

/* `tty` and `ls` ask for the terminal's name; wk gives a node one terminal
 * with no path, so report "not a tty" rather than inventing /dev/tty. */
char *ttyname(int fd);
int ttyname_r(int fd, char *buf, size_t buflen);

/* No filesystem root switching, sessions, or process groups in WASI. */
int chroot(const char *path);
pid_t setsid(void);
pid_t getpgrp(void);
pid_t getppid(void);

/* No pipes in WASI (they exist only between processes, which WASI lacks).
 * Declared so gnulib's pipe module compiles; fails with ENOSYS. */
int pipe(int fds[2]);
int pipe2(int fds[2], int flags);

/* No process creation in WASI; declared so the programs that reference them
 * compile (they fail with ENOSYS at runtime). */
/* WASI has no descriptor duplication (no dup/dup2 in wasi-libc); compat.c
 * fails them with ENOSYS. Only the shell-ish redirection paths want these,
 * which this build doesn't use. */
int dup(int fd);
int dup2(int oldfd, int newfd);

pid_t fork(void);
int execvp(const char *file, char *const argv[]);
int execlp(const char *file, const char *arg, ...);
int execl(const char *path, const char *arg, ...);
int execve(const char *path, char *const argv[], char *const envp[]);

#endif
