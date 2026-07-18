/* <pwd.h> for wasm32-wasi: no user database exists in the sandbox, so lookups
 * report "no such user" (NULL) and Vim falls back cleanly (e.g. ~user
 * expansion just fails, plain ~ uses $HOME). Needed by the normal-feature
 * build (fileio.c and friends). */
#ifndef _WK_COMPAT_PWD_H
#define _WK_COMPAT_PWD_H

#include <sys/types.h>

struct passwd {
    char *pw_name;
    char *pw_passwd;
    uid_t pw_uid;
    gid_t pw_gid;
    char *pw_gecos;
    char *pw_dir;
    char *pw_shell;
};

static inline struct passwd *getpwuid(uid_t uid) {
    (void)uid;
    return 0;
}
static inline struct passwd *getpwnam(const char *name) {
    (void)name;
    return 0;
}
static inline struct passwd *getpwent(void) {
    return 0;
}
static inline void setpwent(void) {}
static inline void endpwent(void) {}

#endif
