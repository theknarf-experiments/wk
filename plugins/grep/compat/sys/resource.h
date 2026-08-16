/* <sys/resource.h> wrapper for wasm32-wasi.
 *
 * wasi-libc ships this header but keeps its whole body behind
 * `__wasilibc_unmodified_upstream` — no rlimits exist in WASI. gnulib's
 * getdtablesize replacement needs RLIMIT_NOFILE and struct rlimit to compile,
 * so supply them (guarded on RLIMIT_NOFILE, so this never double-defines if a
 * future sysroot provides the real thing) and report "unlimited", which sends
 * gnulib to its own sensible default.
 */
#ifndef _WK_COMPAT_SYS_RESOURCE_H
#define _WK_COMPAT_SYS_RESOURCE_H

#include_next <sys/resource.h>

/* Guarded on RLIMIT_DATA because that is the sentinel coreutils itself probes
 * (sort.c defines a stand-in `struct rlimit` when it is absent, which would
 * then clash with this one). */
#ifndef RLIMIT_DATA
#define RLIMIT_CPU 0
#define RLIMIT_FSIZE 1
#define RLIMIT_DATA 2
#define RLIMIT_STACK 3
#define RLIMIT_CORE 4
#define RLIMIT_NOFILE 7
#define RLIMIT_AS 9
#define RLIM_INFINITY (~0ULL)

typedef unsigned long long rlim_t;

struct rlimit {
  rlim_t rlim_cur;
  rlim_t rlim_max;
};

int getrlimit(int resource, struct rlimit *rlim);
int setrlimit(int resource, const struct rlimit *rlim);
#endif

/* Scheduling priority: nothing to schedule in a single wasm guest. `nice`
 * still compiles (it is built even though this configuration doesn't install
 * it) and fails honestly at runtime. */
#ifndef PRIO_PROCESS
#define PRIO_PROCESS 0
#define PRIO_PGRP 1
#define PRIO_USER 2
int getpriority(int which, id_t who);
int setpriority(int which, id_t who, int prio);
#endif

#endif
