// wasi-libc gates struct sigaction + the sigset ops behind
// __wasilibc_unmodified_upstream (off by default). Signals are inert on a
// single-threaded wasm guest; provide the shapes + no-op ops so code that
// installs handlers compiles (and does nothing at runtime).
#ifndef _WASI_SIGNAL_COMPAT_H
#define _WASI_SIGNAL_COMPAT_H
#if defined(__wasi__)
#define __NEED_sigset_t
#include <__typedef_sigset_t.h>
#include <signal.h>
#ifndef SA_SIGINFO
#define SA_SIGINFO 4
#endif
#ifndef SA_RESTART
#define SA_RESTART 0x10000000
#endif
#ifndef SIG_BLOCK
#define SIG_BLOCK 0
#define SIG_UNBLOCK 1
#define SIG_SETMASK 2
#endif
typedef struct { int si_signo, si_code, si_errno; void *si_addr; int si_pid; } wasi_siginfo_t;
#ifndef __wasi_has_sigaction
#define __wasi_has_sigaction 1
struct sigaction {
    union { void (*sa_handler)(int); void (*sa_sigaction)(int, wasi_siginfo_t*, void*); } __sa_handler;
    sigset_t sa_mask;
    int sa_flags;
    void (*sa_restorer)(void);
};
#define sa_handler   __sa_handler.sa_handler
#define sa_sigaction __sa_handler.sa_sigaction
#ifdef __cplusplus
extern "C" {
#endif
static inline int sigaction(int s, const struct sigaction* a, struct sigaction* o){(void)s;(void)a;(void)o;return 0;}
static inline int sigfillset(sigset_t* s){if(s)*s=(sigset_t)-1;return 0;}
static inline int sigemptyset(sigset_t* s){if(s)*s=0;return 0;}
static inline int sigaddset(sigset_t* s,int n){(void)s;(void)n;return 0;}
static inline int sigprocmask(int h,const sigset_t* s,sigset_t* o){(void)h;(void)s;(void)o;return 0;}
static inline int pthread_sigmask(int h,const sigset_t* s,sigset_t* o){(void)h;(void)s;(void)o;return 0;}
#ifdef __cplusplus
}
#endif
#endif
#endif
#endif
