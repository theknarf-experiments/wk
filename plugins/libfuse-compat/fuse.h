/* Portable libfuse (high-level API, v3) for wasm32-wasi, backed by wk's
 * `wk:fs/provider` capability.
 *
 * This is the ONLY place that knows the capability exists: it presents the
 * libfuse surface an unmodified FUSE daemon expects — `fuse_main`, `struct
 * fuse_operations`, the option parser — and maps it onto wk's serve loop.
 * A daemon's `fuse_main` never returns to mount a kernel filesystem; it
 * *becomes* the loop answering other nodes' filesystem operations, exactly
 * as a real daemon answers the kernel through /dev/fuse. The mountpoint
 * argument is accepted and ignored: in wk, where a filesystem mounts is the
 * consumer's wire, not the daemon's argv.
 *
 * Scope: the path-based high-level API that simple, self-contained FUSE
 * filesystems use (hello, memfs, archive views). Field names and signatures
 * follow libfuse 3.x, so sources using designated initializers compile
 * unmodified. Callbacks wk's provider protocol never issues (xattrs, locks,
 * poll, …) are declared for source compatibility and never called.
 */
#ifndef _FUSE_H_
#define _FUSE_H_

#include <stddef.h>
#include <stdint.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/types.h>
#include <time.h>

#ifdef __cplusplus
extern "C" {
#endif

#define FUSE_MAJOR_VERSION 3
#define FUSE_MINOR_VERSION 16
#define FUSE_VERSION (FUSE_MAJOR_VERSION * 100 + FUSE_MINOR_VERSION)

#ifndef FUSE_USE_VERSION
#error "define FUSE_USE_VERSION before including fuse.h"
#endif

/* ---- POSIX bits wasi-libc doesn't declare ----
 *
 * Daemons like passthrough.c call these unconditionally; wasi-libc has no
 * declarations (or definitions) for them. Declared here — fuse.h is the
 * first include in a FUSE daemon — with shim stubs that fail with ENOSYS
 * (umask is an accepted no-op). wk's provider protocol never issues the
 * operations that would reach them, so daemons behave as if the underlying
 * filesystem simply doesn't support device nodes or ownership. */
int lchown(const char *path, uid_t uid, gid_t gid);
int mkfifoat(int dirfd, const char *path, mode_t mode);
int mknodat(int dirfd, const char *path, mode_t mode, dev_t dev);
mode_t umask(mode_t mask);

/* ---- fuse_common.h surface ---- */

struct fuse_file_info {
    int flags; /* open(2) flags: the shim sets O_RDONLY, or O_RDWR|O_CREAT|
                  O_TRUNC as implied by the consumer's open */
    unsigned int writepage : 1;
    unsigned int direct_io : 1;
    unsigned int keep_cache : 1;
    unsigned int flush : 1;
    unsigned int nonseekable : 1;
    unsigned int flock_release : 1;
    unsigned int cache_readdir : 1;
    unsigned int noflush : 1;
    unsigned int parallel_direct_writes : 1;
    unsigned int padding : 23;
    uint64_t fh; /* the filesystem's own handle, set in open/create */
    uint64_t lock_owner;
    uint32_t poll_events;
};

struct fuse_conn_info {
    unsigned proto_major;
    unsigned proto_minor;
    unsigned max_write;
    unsigned max_read;
    unsigned max_readahead;
    unsigned capable;
    unsigned want;
    unsigned max_background;
    unsigned congestion_threshold;
    unsigned time_gran;
    unsigned reserved[22];
};

/* ---- fuse_opt.h surface ---- */

struct fuse_args {
    int argc;
    char **argv;
    int allocated;
};

#define FUSE_ARGS_INIT(argc, argv) \
    { argc, argv, 0 }

struct fuse_opt {
    const char *templ;
    unsigned long offset;
    int value;
};

#define FUSE_OPT_KEY(templ, key) \
    { templ, (unsigned long)-1, key }
#define FUSE_OPT_END \
    { NULL, 0, 0 }

#define FUSE_OPT_KEY_OPT -1
#define FUSE_OPT_KEY_NONOPT -2
#define FUSE_OPT_KEY_KEEP -3
#define FUSE_OPT_KEY_DISCARD -4

typedef int (*fuse_opt_proc_t)(void *data, const char *arg, int key,
                               struct fuse_args *outargs);

int fuse_opt_parse(struct fuse_args *args, void *data,
                   const struct fuse_opt opts[], fuse_opt_proc_t proc);
int fuse_opt_add_arg(struct fuse_args *args, const char *arg);
void fuse_opt_free_args(struct fuse_args *args);

/* ---- fuse.h (high-level API) surface ---- */

struct fuse; /* opaque */

struct fuse_config {
    int set_gid;
    unsigned int gid;
    int set_uid;
    unsigned int uid;
    int set_mode;
    unsigned int umask;
    double entry_timeout;
    double negative_timeout;
    double attr_timeout;
    int intr;
    int intr_signal;
    int remember;
    int hard_remove;
    int use_ino;
    int readdir_ino;
    int direct_io;
    int kernel_cache;
    int auto_cache;
    int no_rofd_flush;
    int ac_attr_timeout_set;
    double ac_attr_timeout;
    int nullpath_ok;
    int show_help;
    char *modules;
    int debug;
};

enum fuse_readdir_flags {
    FUSE_READDIR_PLUS = (1 << 0),
};

enum fuse_fill_dir_flags {
    FUSE_FILL_DIR_PLUS = (1 << 1),
};

typedef int (*fuse_fill_dir_t)(void *buf, const char *name,
                               const struct stat *stbuf, off_t off,
                               enum fuse_fill_dir_flags flags);

struct fuse_operations {
    int (*getattr)(const char *, struct stat *, struct fuse_file_info *);
    int (*readlink)(const char *, char *, size_t);
    int (*mknod)(const char *, mode_t, dev_t);
    int (*mkdir)(const char *, mode_t);
    int (*unlink)(const char *);
    int (*rmdir)(const char *);
    int (*symlink)(const char *, const char *);
    int (*rename)(const char *, const char *, unsigned int);
    int (*link)(const char *, const char *);
    int (*chmod)(const char *, mode_t, struct fuse_file_info *);
    int (*chown)(const char *, uid_t, gid_t, struct fuse_file_info *);
    int (*truncate)(const char *, off_t, struct fuse_file_info *);
    int (*open)(const char *, struct fuse_file_info *);
    int (*read)(const char *, char *, size_t, off_t, struct fuse_file_info *);
    int (*write)(const char *, const char *, size_t, off_t,
                 struct fuse_file_info *);
    int (*statfs)(const char *, struct statvfs *);
    int (*flush)(const char *, struct fuse_file_info *);
    int (*release)(const char *, struct fuse_file_info *);
    int (*fsync)(const char *, int, struct fuse_file_info *);
    int (*setxattr)(const char *, const char *, const char *, size_t, int);
    int (*getxattr)(const char *, const char *, char *, size_t);
    int (*listxattr)(const char *, char *, size_t);
    int (*removexattr)(const char *, const char *);
    int (*opendir)(const char *, struct fuse_file_info *);
    int (*readdir)(const char *, void *, fuse_fill_dir_t, off_t,
                   struct fuse_file_info *, enum fuse_readdir_flags);
    int (*releasedir)(const char *, struct fuse_file_info *);
    int (*fsyncdir)(const char *, int, struct fuse_file_info *);
    void *(*init)(struct fuse_conn_info *conn, struct fuse_config *cfg);
    void (*destroy)(void *private_data);
    int (*access)(const char *, int);
    int (*create)(const char *, mode_t, struct fuse_file_info *);
    int (*utimens)(const char *, const struct timespec tv[2],
                   struct fuse_file_info *);
    int (*fallocate)(const char *, int, off_t, off_t,
                     struct fuse_file_info *);
    off_t (*lseek)(const char *, off_t, int, struct fuse_file_info *);
};

struct fuse_context {
    struct fuse *fuse;
    uid_t uid;
    gid_t gid;
    pid_t pid;
    void *private_data;
    mode_t umask;
};

struct fuse_context *fuse_get_context(void);

int fuse_main_real(int argc, char *argv[], const struct fuse_operations *op,
                   size_t op_size, void *private_data);

#define fuse_main(argc, argv, op, private_data) \
    fuse_main_real(argc, argv, op, sizeof(*(op)), private_data)

#ifdef __cplusplus
}
#endif

#endif /* _FUSE_H_ */
