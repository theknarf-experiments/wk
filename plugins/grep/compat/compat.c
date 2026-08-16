/* The handful of calls WASI lacks that GNU grep (through gnulib) references at
 * link time. Each is inert: a wasm guest has one file table, no other
 * processes, and nothing to schedule, so the honest answers are "unlimited"
 * and "nothing happened". compat/sys/resource.h declares the rlimit set; here
 * are the bodies so the module links. (coreutils supplies the same from its
 * own, larger compat.c — grep only needs this corner of it.) */
#include <errno.h>
#include <stdio.h>
#include <sys/resource.h>

int getrlimit(int resource, struct rlimit *rlim) {
    (void)resource;
    if (rlim) {
        rlim->rlim_cur = RLIM_INFINITY;
        rlim->rlim_max = RLIM_INFINITY;
    }
    return 0;
}

int setrlimit(int resource, const struct rlimit *rlim) {
    (void)resource;
    (void)rlim;
    return 0;
}

int getpriority(int which, id_t who) {
    (void)which;
    (void)who;
    return 0;
}

int setpriority(int which, id_t who, int prio) {
    (void)which;
    (void)who;
    (void)prio;
    errno = ENOSYS;
    return -1;
}

/* gnulib's getopt locks stdio per-FILE; wasi-libc has one thread and no
 * flockfile, so these are no-ops. */
void flockfile(FILE *f) { (void)f; }
void funlockfile(FILE *f) { (void)f; }
int ftrylockfile(FILE *f) {
    (void)f;
    return 0;
}
