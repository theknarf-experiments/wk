/* Minimal <mntent.h> for WASI.
 *
 * gnulib's mountlist (used by df, and required unconditionally by coreutils'
 * configure) is built around the glibc mtab reader. WASI has no mount table at
 * all — a wk node's filesystem is a private in-memory vfs — so this header
 * supplies the shape gnulib compiles against and compat.c implements it as an
 * empty table: `df` builds, runs, and honestly reports no mounted filesystems
 * instead of the build failing.
 */
#ifndef _WK_COMPAT_MNTENT_H
#define _WK_COMPAT_MNTENT_H

#include <stdio.h>

#define MOUNTED "/etc/mtab"

struct mntent {
  char *mnt_fsname;
  char *mnt_dir;
  char *mnt_type;
  char *mnt_opts;
  int mnt_freq;
  int mnt_passno;
};

FILE *setmntent(const char *filename, const char *type);
struct mntent *getmntent(FILE *stream);
int endmntent(FILE *stream);
char *hasmntopt(const struct mntent *mnt, const char *opt);

#endif
