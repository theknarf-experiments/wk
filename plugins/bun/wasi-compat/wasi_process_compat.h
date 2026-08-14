// wasi has no process credentials / signals / umask. Inert stubs so
// process.* bindings compile (they fail or no-op at runtime on wasm).
#ifndef _WASI_PROCESS_COMPAT_H
#define _WASI_PROCESS_COMPAT_H
#if defined(__wasi__)
#include <sys/types.h>
#include <errno.h>
#include <stdint.h>
#ifndef RLIMIT_CPU
#define RLIMIT_CPU 0
#define RLIMIT_FSIZE 1
#define RLIMIT_DATA 2
#define RLIMIT_STACK 3
#define RLIMIT_CORE 4
#define RLIMIT_RSS 5
#define RLIMIT_NPROC 6
#define RLIMIT_NOFILE 7
#define RLIMIT_MEMLOCK 8
#define RLIMIT_AS 9
#define RLIM_INFINITY (~0ULL)
typedef uint64_t rlim_t;
struct rlimit { rlim_t rlim_cur; rlim_t rlim_max; };
// wasi's active `struct rusage` is minimal (utime/stime only); the BSD-full
// shape lets process.cpuUsage/resourceUsage compile (all-zero on wasm).
struct BunWasiRusage {
    struct { long tv_sec, tv_usec; } ru_utime, ru_stime;
    long ru_maxrss, ru_ixrss, ru_idrss, ru_isrss, ru_minflt, ru_majflt,
         ru_nswap, ru_inblock, ru_oublock, ru_msgsnd, ru_msgrcv,
         ru_nsignals, ru_nvcsw, ru_nivcsw;
};
#endif
#ifdef __cplusplus
extern "C" {
#endif
static inline unsigned umask(unsigned m){(void)m;return 0;}
static inline uid_t getuid(void){return 0;}
static inline uid_t geteuid(void){return 0;}
static inline gid_t getgid(void){return 0;}
static inline gid_t getegid(void){return 0;}
static inline int getrlimit(int r,struct rlimit*l){(void)r;if(l){l->rlim_cur=RLIM_INFINITY;l->rlim_max=RLIM_INFINITY;}return 0;}
static inline int setrlimit(int r,const struct rlimit*l){(void)r;(void)l;errno=ENOSYS;return -1;}
static inline int setuid(uid_t u){(void)u;errno=ENOSYS;return -1;}
static inline int setgid(gid_t g){(void)g;errno=ENOSYS;return -1;}
static inline int seteuid(uid_t u){(void)u;errno=ENOSYS;return -1;}
static inline int setegid(gid_t g){(void)g;errno=ENOSYS;return -1;}
static inline int getgroups(int n,gid_t*g){(void)n;(void)g;return 0;}
static inline int setgroups(size_t n,const gid_t*g){(void)n;(void)g;errno=ENOSYS;return -1;}
static inline int initgroups(const char*u,gid_t g){(void)u;(void)g;errno=ENOSYS;return -1;}
static inline pid_t getppid(void){return 1;}
static inline int kill(pid_t p,int s){(void)p;(void)s;errno=ENOSYS;return -1;}
static inline int execve(const char*p,char*const a[],char*const e[]){(void)p;(void)a;(void)e;errno=ENOSYS;return -1;}
static inline int execvp(const char*f,char*const a[]){(void)f;(void)a;errno=ENOSYS;return -1;}
static inline pid_t fork(void){errno=ENOSYS;return -1;}
static inline pid_t waitpid(pid_t p,int*s,int o){(void)p;(void)s;(void)o;errno=ENOSYS;return -1;}
#ifdef __cplusplus
}
#endif
#endif
#endif
