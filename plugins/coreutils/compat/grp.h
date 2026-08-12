/* <grp.h> for wasm32-wasi: like the pwd.h shim, there is no group database in
 * the sandbox — compat.c reports a single synthetic "wk" group. */
#ifndef _WK_COMPAT_GRP_H
#define _WK_COMPAT_GRP_H

#include <sys/types.h>

struct group {
  char *gr_name;
  char *gr_passwd;
  gid_t gr_gid;
  char **gr_mem;
};

struct group *getgrgid(gid_t gid);
struct group *getgrnam(const char *name);
struct group *getgrent(void);
void setgrent(void);
void endgrent(void);
int getgroups(int size, gid_t list[]);
int initgroups(const char *user, gid_t group);

#endif
