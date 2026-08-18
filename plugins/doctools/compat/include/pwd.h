/* Stub <pwd.h> for the pdftex cross build: no users on WASI. xpdf's goo/gfile
 * and kpathsea's tilde expansion compile against these; the lookups (defined
 * in wasi-shim.c) always answer "no such user", so ~ stays unexpanded and
 * home-relative paths fall back — which is fine inside a container whose HOME
 * is an ordinary env var.
 */
#ifndef WK_DOCTOOLS_PWD_H
#define WK_DOCTOOLS_PWD_H

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

struct passwd {
    char *pw_name;
    char *pw_passwd;
    uid_t pw_uid;
    gid_t pw_gid;
    char *pw_gecos;
    char *pw_dir;
    char *pw_shell;
};

extern struct passwd *getpwnam(const char *name);
extern struct passwd *getpwuid(uid_t uid);

#ifdef __cplusplus
}
#endif

#endif
