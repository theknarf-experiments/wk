// wasi-emulated-signal lacks the sigset_t operations; provide inert stubs.
#pragma once
#if defined(__wasi__)
#include <signal.h>
#ifdef __cplusplus
extern "C" {
#endif
#ifndef sigfillset
static inline int sigfillset(sigset_t *s) { if (s) *s = (sigset_t)-1; return 0; }
#endif
#ifndef sigemptyset
static inline int sigemptyset(sigset_t *s) { if (s) *s = 0; return 0; }
#endif
#ifndef sigaddset
static inline int sigaddset(sigset_t *s, int n) { (void)n; (void)s; return 0; }
#endif
#ifndef sigprocmask
static inline int sigprocmask(int h, const sigset_t *s, sigset_t *o) { (void)h;(void)s;(void)o; return 0; }
#endif
#ifdef __cplusplus
}
#endif
#endif
