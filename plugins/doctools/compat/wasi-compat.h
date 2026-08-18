/* Forced include (-include) for the pdftex cross build: compile-time paper
 * over things wasi-libc doesn't declare.
 *
 * kpathsea's line.c locks the stream around getc_unlocked *unconditionally*
 * ("perhaps we will be lucky enough to do this unconditionally" — not on
 * WASI). A wasm guest is single-threaded, so the lock is a no-op and the
 * unlocked getc is just getc.
 */
#ifndef WK_DOCTOOLS_WASI_COMPAT_H
#define WK_DOCTOOLS_WASI_COMPAT_H

#define flockfile(f) ((void)(f))
#define funlockfile(f) ((void)(f))
#define getc_unlocked getc

/* wasi-libc has no per-thread CPU clock (there is one thread). texprof asks
 * for one; the monotonic clock is the honest substitute. Expands at the use
 * site, after <time.h> has defined CLOCK_MONOTONIC. */
#define CLOCK_THREAD_CPUTIME_ID CLOCK_MONOTONIC

/* Process-spawning declarations wasi-libc's unistd.h leaves out (no processes
 * on WASI). kpathsea's mktex fallback compiles against these; at runtime the
 * wasi-shim.c definitions fail with ENOSYS, which kpathsea treats as "font
 * generation unavailable" — the right answer in a sandbox.
 */
#ifdef __cplusplus
extern "C" {
#endif
extern int execv(const char *path, char *const argv[]);
extern int execvp(const char *file, char *const argv[]);
extern int getuid(void);
extern int geteuid(void);
extern int getgid(void);
extern int getegid(void);
/* stdio.h is not yet included when this header is forced in; wasi-libc is
 * musl-derived, where FILE is struct _IO_FILE, so declare against that. The
 * definitions (wasi-shim.c) fail with ENOSYS: no processes to pipe to. */
struct _IO_FILE;
extern struct _IO_FILE *popen(const char *cmd, const char *mode);
extern int pclose(struct _IO_FILE *f);
#ifdef __cplusplus
}
#endif

#endif
