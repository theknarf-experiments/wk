/* <pwd.h> for wasm32-wasi: WASI has no user database. coreutils needs the
 * shape (ls -l, id, whoami, install, chown all include it); compat.c backs it
 * with a single synthetic "wk" user so those tools print something coherent
 * instead of failing. */
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

struct passwd *getpwuid(uid_t uid);
struct passwd *getpwnam(const char *name);
struct passwd *getpwent(void);
void setpwent(void);
void endpwent(void);

#endif
