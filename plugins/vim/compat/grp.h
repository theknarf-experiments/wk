/* <grp.h> for wasm32-wasi: like the pwd.h shim, there is no group database in
 * the sandbox — lookups report "no such group" and Vim skips group-preserving
 * chores it can't do anyway. */
#ifndef _WK_COMPAT_GRP_H
#define _WK_COMPAT_GRP_H

#include <sys/types.h>

struct group {
    char *gr_name;
    char *gr_passwd;
    gid_t gr_gid;
    char **gr_mem;
};

static inline struct group *getgrgid(gid_t gid) {
    (void)gid;
    return 0;
}
static inline struct group *getgrnam(const char *name) {
    (void)name;
    return 0;
}

#endif
