/* <signal.h> wrapper for wasm32-wasi.
 *
 * wasi-libc keeps the signal-set API behind `__wasilibc_unmodified_upstream`
 * (WASI delivers no signals) and implements only signal()/raise(). coreutils
 * needs the blocking API: sort and csplit guard their temp-file cleanup with
 * it, and — crucially — sort.c *redefines* `sigset_t` to `int` when the
 * platform lacks POSIX signal blocking, which then clashes with the real
 * sigset_t everywhere else.
 *
 * So declare the API (compat.c implements it as accepted-but-inert, which is
 * exactly right when nothing can interrupt a wasm guest) and let configure see
 * it, keeping one consistent sigset_t across the build.
 */
#ifndef _WK_COMPAT_SIGNAL_H
#define _WK_COMPAT_SIGNAL_H

#include <sys/types.h>
#include_next <signal.h>

#ifndef SIG_BLOCK
#define SIG_BLOCK 0
#endif
#ifndef SIG_UNBLOCK
#define SIG_UNBLOCK 1
#endif
#ifndef SIG_SETMASK
#define SIG_SETMASK 2
#endif

/* coreutils (sort.c) probes SA_NOCLDSTOP as "does this platform have the
 * sigaction machinery?" and, when it is missing, redefines sigset_t to int —
 * which then collides with the real sigset_t used everywhere else. Defining
 * the flag makes sort take the normal path; gnulib supplies `struct
 * sigaction` and a sigaction() replacement of its own (the system has
 * neither), so nothing else is needed here.
 */
#ifndef SA_NOCLDSTOP
#define SA_NOCLDSTOP 1
#endif
#ifndef SA_RESTART
#define SA_RESTART 0x10000000
#endif

/* gnulib's own sigaction replacement refuses to build where SIGCHLD exists
 * (WASI defines the number, though no child can ever raise it), so the build
 * tells configure the system has sigaction and supplies it here instead —
 * inert, like the rest of the signal API. */
/* gnulib/bash both want the sigaction machinery. wasi-libc has none, so
 * declare it here and let compat.c implement it inertly — nothing can deliver
 * a signal to a wasm guest. Plain members (not the glibc union + macros): the
 * `#define sa_handler ...` trick breaks unrelated declarations in bash. */
#ifndef _WK_HAVE_STRUCT_SIGACTION
#define _WK_HAVE_STRUCT_SIGACTION 1
struct sigaction {
  void (*sa_handler)(int);
  void (*sa_sigaction)(int, void *, void *);
  sigset_t sa_mask;
  int sa_flags;
  void (*sa_restorer)(void);
};

int sigaction(int, const struct sigaction *__restrict, struct sigaction *__restrict);
#endif

int sigemptyset(sigset_t *);
int sigfillset(sigset_t *);
int sigaddset(sigset_t *, int);
int sigdelset(sigset_t *, int);
int sigismember(const sigset_t *, int);
int sigprocmask(int, const sigset_t *__restrict, sigset_t *__restrict);
int sigsuspend(const sigset_t *);

/* No timers in WASI: alarm() never fires. */
unsigned int alarm(unsigned int seconds);

/* tail --pid= checks whether a writer is still alive; with no processes the
 * honest answer is "no such process". */
int kill(pid_t, int);
int raise(int);

#endif
