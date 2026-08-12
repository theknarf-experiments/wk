/* WASI compatibility shims for upstream GNU coreutils.
 *
 * Nothing here patches coreutils: these are the POSIX corners WASI omits,
 * supplied at link time the way plugins/vim/compat/wkos.c does it.
 *
 * 2. Process/user identity: WASI has no users, groups, or processes. The
 *    stubs report a single "wk" user (uid/gid 0) so `ls -l`, `id`, `whoami`,
 *    and `stat` print something coherent rather than failing.
 * 3. umask/tzset: no file-creation mask and no timezone database in WASI.
 *    (Signal blocking is left to gnulib's own emulation; see compat/signal.h.)
 */

#include <errno.h>
#include <stdio.h>
#include <grp.h>
#include <pwd.h>
#include <stddef.h>
#include <string.h>
#include <sys/types.h>

/* ---- users and groups: one synthetic entry ---- */

static char wk_name[] = "wk";
static char wk_dir[] = "/";
static char wk_shell[] = "/bin/sh";
static char wk_passwd[] = "x";

static struct passwd wk_pw = {
    .pw_name = wk_name,
    .pw_passwd = wk_passwd,
    .pw_uid = 0,
    .pw_gid = 0,
    .pw_dir = wk_dir,
    .pw_shell = wk_shell,
};

static char *wk_members[] = {wk_name, NULL};

static struct group wk_gr = {
    .gr_name = wk_name,
    .gr_passwd = wk_passwd,
    .gr_gid = 0,
    .gr_mem = wk_members,
};

struct passwd *getpwuid(uid_t uid) { return uid == 0 ? &wk_pw : NULL; }

struct passwd *getpwnam(const char *name) {
  return name && strcmp(name, wk_name) == 0 ? &wk_pw : NULL;
}

struct group *getgrgid(gid_t gid) { return gid == 0 ? &wk_gr : NULL; }

struct group *getgrnam(const char *name) {
  return name && strcmp(name, wk_name) == 0 ? &wk_gr : NULL;
}

void setpwent(void) {}
void endpwent(void) {}
struct passwd *getpwent(void) { return NULL; }
void setgrent(void) {}
void endgrent(void) {}
struct group *getgrent(void) { return NULL; }

int getgroups(int size, gid_t list[]) {
  (void)size;
  (void)list;
  return 0;
}

int initgroups(const char *user, gid_t group) {
  (void)user;
  (void)group;
  return 0;
}

uid_t getuid(void) { return 0; }
uid_t geteuid(void) { return 0; }
gid_t getgid(void) { return 0; }
gid_t getegid(void) { return 0; }

/* WASI has no timezone database — everything is UTC, so this is a no-op. */
void tzset(void) {}

/* WASI has no file-creation mask; keep one locally so umask() round-trips
 * (nothing in the vfs consults it — modes are advisory here). */
#include <sys/stat.h>
static mode_t wk_umask = 022;
mode_t umask(mode_t mask) {
  mode_t old = wk_umask;
  wk_umask = mask;
  return old;
}

/* ---- processes: WASI has none ----
 *
 * A wasm guest cannot fork or exec. coreutils only reaches these in the
 * programs that spawn helpers (install --strip, and the env/nohup/timeout
 * family, which this build excludes); failing with ENOSYS keeps the rest of
 * the multicall binary honest instead of failing the build. */
#include <sys/resource.h>
#include <sys/wait.h>

/* dup/dup2: bash supplies its own in lib/sh/oslib.c — don't collide. */

char *ttyname(int fd) { (void)fd; errno = ENOTTY; return NULL; }
int ttyname_r(int fd, char *buf, size_t buflen) {
  (void)fd; (void)buf; (void)buflen;
  return ENOTTY;
}

int chroot(const char *path) { (void)path; errno = ENOSYS; return -1; }
pid_t setsid(void) { errno = ENOSYS; return -1; }
pid_t getpgrp(void) { return 1; }
pid_t getppid(void) { return 0; }

int getpriority(int which, id_t who) { (void)which; (void)who; return 0; }
int setpriority(int which, id_t who, int prio) {
  (void)which; (void)who; (void)prio;
  errno = ENOSYS;
  return -1;
}

/* pipe()/pipe2() are real now — see ../pipe-compat, which is linked in and
   backs them with wk's pipe. They used to be stubbed here. */

pid_t fork(void) { errno = ENOSYS; return -1; }
pid_t wait(int *status) { (void)status; errno = ECHILD; return -1; }
pid_t waitpid(pid_t pid, int *status, int options) {
  (void)pid; (void)status; (void)options;
  errno = ECHILD;
  return -1;
}
int execvp(const char *file, char *const argv[]) {
  (void)file; (void)argv;
  errno = ENOSYS;
  return -1;
}
int execlp(const char *file, const char *arg, ...) {
  (void)file; (void)arg;
  errno = ENOSYS;
  return -1;
}
int execve(const char *path, char *const argv[], char *const envp[]) {
  (void)path; (void)argv; (void)envp;
  errno = ENOSYS;
  return -1;
}

/* ---- signal blocking: accepted, never delivered ----
 *
 * See compat/signal.h: coreutils' critical sections need this API to exist,
 * and nothing can deliver a signal to a wasm guest, so the masks are inert. */
#include <signal.h>

int sigprocmask(int how, const sigset_t *restrict set, sigset_t *restrict old) {
  (void)how; (void)set;
  if (old) memset(old, 0, sizeof *old);
  return 0;
}
int sigemptyset(sigset_t *set) { if (set) memset(set, 0, sizeof *set); return 0; }
int sigfillset(sigset_t *set) { if (set) memset(set, 0xff, sizeof *set); return 0; }
int sigaddset(sigset_t *set, int sig) { (void)set; (void)sig; return 0; }
int sigdelset(sigset_t *set, int sig) { (void)set; (void)sig; return 0; }
int sigismember(const sigset_t *set, int sig) { (void)set; (void)sig; return 0; }
int sigsuspend(const sigset_t *set) { (void)set; errno = EINTR; return -1; }
unsigned int alarm(unsigned int seconds) { (void)seconds; return 0; }
int kill(pid_t pid, int sig) { (void)pid; (void)sig; errno = ESRCH; return -1; }

int sigaction(int sig, const struct sigaction *restrict act,
              struct sigaction *restrict old) {
  (void)sig; (void)act;
  if (old) memset(old, 0, sizeof *old);
  return 0;
}

int execl(const char *path, const char *arg, ...) {
  (void)path; (void)arg;
  errno = ENOSYS;
  return -1;
}

/* ---- resource limits: none in WASI ---- */
#include <sys/resource.h>

int getrlimit(int resource, struct rlimit *rlim) {
  if (!rlim)
    return 0;
  /* Report modest, *finite* limits. Several coreutils size their working
   * buffers from these (sort's merge fan-in, ls's allocation heuristics), and
   * RLIM_INFINITY makes that arithmetic produce absurd values — the symptom is
   * "memory exhausted" before any work happens. wasm32 has a 4 GiB address
   * space and wk gives a node far less, so these are honest numbers. */
  switch (resource) {
  case RLIMIT_NOFILE:
    rlim->rlim_cur = 1024;
    rlim->rlim_max = 1024;
    break;
  case RLIMIT_DATA:
  case RLIMIT_AS:
    rlim->rlim_cur = 256u * 1024 * 1024;
    rlim->rlim_max = 256u * 1024 * 1024;
    break;
  case RLIMIT_STACK:
    rlim->rlim_cur = 8u * 1024 * 1024;
    rlim->rlim_max = 8u * 1024 * 1024;
    break;
  default:
    rlim->rlim_cur = 256u * 1024 * 1024;
    rlim->rlim_max = 256u * 1024 * 1024;
    break;
  }
  return 0;
}

int setrlimit(int resource, const struct rlimit *rlim) {
  (void)resource; (void)rlim;
  return 0;
}

/* ---- ownership: the vfs has no uid/gid ---- */
int chown(const char *path, uid_t owner, gid_t group) {
  (void)path; (void)owner; (void)group;
  errno = ENOSYS;
  return -1;
}
int fchown(int fd, uid_t owner, gid_t group) {
  (void)fd; (void)owner; (void)group;
  errno = ENOSYS;
  return -1;
}
int lchown(const char *path, uid_t owner, gid_t group) {
  (void)path; (void)owner; (void)group;
  errno = ENOSYS;
  return -1;
}
int fchownat(int fd, const char *path, uid_t owner, gid_t group, int flag) {
  (void)fd; (void)path; (void)owner; (void)group; (void)flag;
  errno = ENOSYS;
  return -1;
}

/* ---- FILE locking: a wasm guest is single-threaded ---- */
void flockfile(FILE *stream) { (void)stream; }
void funlockfile(FILE *stream) { (void)stream; }
int ftrylockfile(FILE *stream) { (void)stream; return 0; }

/* ---- qsort_r: wasi-libc has only plain qsort ----
 *
 * A wasm guest is single-threaded and coreutils never sorts re-entrantly, so
 * stashing the comparator + context in file-scope state is safe. */
#include <stdlib.h>

static int (*wk_qsort_cmp)(const void *, const void *, void *);
static void *wk_qsort_arg;

static int wk_qsort_trampoline(const void *a, const void *b) {
  return wk_qsort_cmp(a, b, wk_qsort_arg);
}

void qsort_r(void *base, size_t nmemb, size_t size,
             int (*compar)(const void *, const void *, void *), void *arg) {
  wk_qsort_cmp = compar;
  wk_qsort_arg = arg;
  qsort(base, nmemb, size, wk_qsort_trampoline);
}

/* ---- entry point: bash's main takes three arguments ----
 *
 * clang aliases `main` to wasi-libc's expected `__main_argc_argv` only when it
 * has the standard (int, char **) signature. bash still uses the K&R
 * three-argument form `main (argc, argv, env)`, so the alias isn't made and
 * the reference stays unresolved — the module links but traps immediately at
 * `undefined_weak:main`. (bash's own NO_MAIN_ENV_ARG path drops the parameter
 * but its body still uses `env`, so that isn't a fix.) Provide the adapter
 * here instead of patching bash. */
extern char **environ;
extern int main(int argc, char **argv, char **env);

int __main_argc_argv(int argc, char **argv) { return main(argc, argv, environ); }

/* ---- setuid/setgid: no users to become ---- */
int setuid(uid_t uid) { return uid == 0 ? 0 : (errno = EPERM, -1); }
int seteuid(uid_t uid) { return uid == 0 ? 0 : (errno = EPERM, -1); }
int setgid(gid_t gid) { return gid == 0 ? 0 : (errno = EPERM, -1); }
int setegid(gid_t gid) { return gid == 0 ? 0 : (errno = EPERM, -1); }

/* ---- dynamic loading: none in WASI ----
 *
 * bash's `enable -f file name` loads builtins from shared objects. A wasm
 * guest has no dynamic linker, so these fail cleanly and `enable -f` reports
 * the error instead of the build failing. */
void *dlopen(const char *file, int mode) {
  (void)file; (void)mode;
  return NULL;
}
void *dlsym(void *handle, const char *name) {
  (void)handle; (void)name;
  return NULL;
}
int dlclose(void *handle) { (void)handle; return 0; }
char *dlerror(void) { return (char *)"dynamic loading is not supported on WASI"; }
