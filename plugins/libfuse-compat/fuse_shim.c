/* The libfuse-compat engine: fuse_main() as a wk:fs/provider serve loop.
 *
 * A FUSE daemon's main() parses options and calls fuse_main(). Upstream, that
 * mounts a kernel filesystem and loops reading /dev/fuse; here it loops on
 * wk_fs_provider_next_request(), dispatching each operation to the daemon's
 * own fuse_operations callbacks and replying. The daemon's code runs
 * unmodified — it cannot tell it isn't talking to a kernel.
 *
 * Protocol mapping notes:
 * - wk:fs paths are provider-root-relative with "" for the root; FUSE paths
 *   are absolute ("/", "/x") — converted at the boundary.
 * - wk:fs `open` carries create/truncate/exclusive but not read-vs-write
 *   intent, so a plain open passes O_RDONLY (what read-only daemons like
 *   hello expect to see) and create/truncate imply O_RDWR. Daemons that
 *   check fi->flags in write() would see O_RDONLY there; the common ones
 *   don't.
 * - `readdir` kind per entry: a stbuf claiming S_IFREG is trusted; every
 *   other claim (or none) is resolved by a getattr per entry — see
 *   dir_fill for why wasi makes directory claims untrustworthy.
 * - FUSE's offset-paged readdir protocol (filler returning 1) is not
 *   driven: the shim always collects the whole listing in one pass, which
 *   is the mode simple daemons use (filler(buf, name, NULL, 0, 0)).
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#define FUSE_USE_VERSION 31

#include "fuse.h"
#include "wkfuse.h" /* wit-bindgen: wk_fs_provider_* */

/* The daemon's callbacks and context, captured by fuse_main_real. */
static struct fuse_operations g_ops;
static struct fuse_context g_ctx;

struct fuse_context *fuse_get_context(void) {
    return &g_ctx;
}

/* ---- option parsing (the subset real daemons use) ---- */

int fuse_opt_add_arg(struct fuse_args *args, const char *arg) {
    char **argv = realloc(
        args->allocated ? args->argv : NULL,
        (size_t)(args->argc + 2) * sizeof(char *));
    if (!argv)
        return -1;
    if (!args->allocated) {
        /* First growth: the original argv was the caller's stack array;
           copy its pointers (dup'ing lazily is unnecessary — parse dups). */
        for (int i = 0; i < args->argc; i++)
            argv[i] = args->argv[i];
    }
    argv[args->argc] = strdup(arg);
    if (!argv[args->argc])
        return -1;
    args->argc++;
    argv[args->argc] = NULL;
    args->argv = argv;
    args->allocated = 1;
    return 0;
}

void fuse_opt_free_args(struct fuse_args *args) {
    if (args && args->allocated) {
        for (int i = 0; i < args->argc; i++)
            free(args->argv[i]);
        free(args->argv);
        args->argv = NULL;
        args->argc = 0;
        args->allocated = 0;
    }
}

/* Match `arg` against one template. Returns 1 on match (and applies the
 * side effect into `data`), 0 otherwise. Handles the two forms simple
 * daemons use: an exact flag ("-h", "--help") setting an int, and a
 * "--name=%s" prefix capturing a strdup'd string. Numeric captures accept
 * %d/%u/%lu via sscanf. */
static int opt_match(const struct fuse_opt *o, const char *arg, void *data) {
    const char *pct = strchr(o->templ, '%');
    if (!pct) {
        if (strcmp(arg, o->templ) != 0)
            return 0;
        if (o->offset != (unsigned long)-1)
            *(int *)((char *)data + o->offset) = o->value;
        return 1;
    }
    size_t fixed = (size_t)(pct - o->templ);
    if (strncmp(arg, o->templ, fixed) != 0)
        return 0;
    const char *rest = arg + fixed;
    if (o->offset == (unsigned long)-1)
        return 1; /* keyed template: matching is all that matters here */
    if (strcmp(pct, "%s") == 0) {
        char **slot = (char **)((char *)data + o->offset);
        free(*slot);
        *slot = strdup(rest);
    } else if (strcmp(pct, "%d") == 0 || strcmp(pct, "%i") == 0) {
        sscanf(rest, "%d", (int *)((char *)data + o->offset));
    } else if (strcmp(pct, "%u") == 0) {
        sscanf(rest, "%u", (unsigned *)((char *)data + o->offset));
    } else if (strcmp(pct, "%lu") == 0) {
        sscanf(rest, "%lu", (unsigned long *)((char *)data + o->offset));
    } else {
        return 0;
    }
    return 1;
}

int fuse_opt_parse(struct fuse_args *args, void *data,
                   const struct fuse_opt opts[], fuse_opt_proc_t proc) {
    if (!args || args->argc == 0)
        return 0;
    struct fuse_args out = FUSE_ARGS_INIT(0, NULL);
    if (fuse_opt_add_arg(&out, args->argv[0]) == -1)
        return -1;
    for (int i = 1; i < args->argc; i++) {
        const char *arg = args->argv[i];
        int matched = 0;
        for (const struct fuse_opt *o = opts; o && o->templ; o++) {
            if (opt_match(o, arg, data)) {
                matched = 1;
                /* Keyed options go through the callback like upstream. */
                if (o->offset == (unsigned long)-1 && proc &&
                    proc(data, arg, o->value, &out) == -1) {
                    fuse_opt_free_args(&out);
                    return -1;
                }
                break;
            }
        }
        if (!matched) {
            int keep = 1;
            if (proc) {
                int key = (arg[0] == '-') ? FUSE_OPT_KEY_OPT
                                          : FUSE_OPT_KEY_NONOPT;
                int r = proc(data, arg, key, &out);
                if (r == -1) {
                    fuse_opt_free_args(&out);
                    return -1;
                }
                keep = (r == 1);
            }
            if (keep && fuse_opt_add_arg(&out, arg) == -1)
                return -1;
        }
    }
    fuse_opt_free_args(args);
    *args = out;
    return 0;
}

/* ---- errno → wk:fs error mapping ---- */

static wk_fs_provider_error_t map_errno(int err) {
    switch (err) {
    case ENOENT:
        return WK_FS_PROVIDER_ERROR_NO_ENTRY;
    case ENOTDIR:
        return WK_FS_PROVIDER_ERROR_NOT_DIR;
    case EISDIR:
        return WK_FS_PROVIDER_ERROR_IS_DIR;
    case EEXIST:
        return WK_FS_PROVIDER_ERROR_EXIST;
    case EACCES:
    case EPERM:
    case EROFS:
        return WK_FS_PROVIDER_ERROR_NOT_PERMITTED;
    case EFBIG:
        return WK_FS_PROVIDER_ERROR_TOO_LARGE;
    case ENOSYS:
        return WK_FS_PROVIDER_ERROR_UNSUPPORTED;
    default:
        return WK_FS_PROVIDER_ERROR_IO;
    }
}

static void reply_err(wk_fs_provider_result_reply_data_error_t *out, int err) {
    out->is_err = true;
    out->val.err = map_errno(err);
}

static void reply_done(wk_fs_provider_result_reply_data_error_t *out) {
    out->is_err = false;
    out->val.ok.tag = WK_FS_PROVIDER_REPLY_DATA_DONE;
}

/* ---- open-handle table ---- */

#define MAX_HANDLES 256

struct handle {
    int used;
    int is_dir;
    char *path; /* FUSE-style, "/x" */
    struct fuse_file_info fi;
};

static struct handle g_handles[MAX_HANDLES];

static uint64_t handle_alloc(const char *path, int is_dir,
                             struct fuse_file_info fi) {
    for (int i = 0; i < MAX_HANDLES; i++) {
        if (!g_handles[i].used) {
            g_handles[i].used = 1;
            g_handles[i].is_dir = is_dir;
            g_handles[i].path = strdup(path);
            g_handles[i].fi = fi;
            return (uint64_t)i + 1;
        }
    }
    return 0;
}

static struct handle *handle_get(uint64_t h) {
    if (h == 0 || h > MAX_HANDLES || !g_handles[h - 1].used)
        return NULL;
    return &g_handles[h - 1];
}

static void handle_free(uint64_t h) {
    struct handle *e = handle_get(h);
    if (e) {
        free(e->path);
        e->path = NULL;
        e->used = 0;
    }
}

/* wk:fs path payload ("a/b", "" = root) → malloc'd FUSE path ("/a/b", "/"). */
static char *fuse_path(const wkfuse_string_t *s) {
    char *p = malloc(s->len + 2);
    if (!p)
        return NULL;
    p[0] = '/';
    memcpy(p + 1, s->ptr, s->len);
    p[s->len + 1] = '\0';
    return p;
}

static int stat_path(const char *path, struct stat *st) {
    memset(st, 0, sizeof(*st));
    if (!g_ops.getattr)
        return -ENOSYS;
    return g_ops.getattr(path, st, NULL);
}

/* ---- readdir collection ---- */

struct dirbuf {
    wk_fs_provider_dirent_t *items;
    size_t len, cap;
    const char *dirpath; /* for per-entry getattr when no stbuf is given */
};

static int dir_fill(void *buf, const char *name, const struct stat *stbuf,
                    off_t off, enum fuse_fill_dir_flags flags) {
    (void)off;
    (void)flags;
    struct dirbuf *b = buf;
    if (strcmp(name, ".") == 0 || strcmp(name, "..") == 0)
        return 0;
    if (b->len == b->cap) {
        size_t cap = b->cap ? b->cap * 2 : 16;
        wk_fs_provider_dirent_t *items =
            realloc(b->items, cap * sizeof(*items));
        if (!items)
            return 1; /* out of memory: tell the daemon to stop */
        b->items = items;
        b->cap = cap;
    }
    wk_fs_provider_entry_kind_t kind = WK_FS_PROVIDER_ENTRY_KIND_FILE;
    /* The daemon's stbuf is only half-trustworthy: the classic
       `st_mode = d_type << 12` readdir idiom (passthrough.c) assumes
       Linux's DT↔S_IF correspondence, and on wasi-libc it produces modes
       that are valid but WRONG — a regular file's d_type (4) lands
       exactly on S_IFDIR. No wasi d_type shifted by 12 can produce
       S_IFREG though, so a claimed file is honest; a claimed directory
       (or anything else) is confirmed by a getattr per entry — an
       in-process callback, no I/O, and dirs are the minority. */
    if (stbuf && S_ISREG(stbuf->st_mode)) {
        kind = WK_FS_PROVIDER_ENTRY_KIND_FILE;
    } else {
        /* The daemon didn't provide attributes (the common `filler(buf,
           name, NULL, 0, 0)` form): ask it. Same process, no I/O. */
        size_t dlen = strlen(b->dirpath), nlen = strlen(name);
        char *child = malloc(dlen + nlen + 2);
        if (child) {
            int sep = dlen > 0 && b->dirpath[dlen - 1] != '/';
            memcpy(child, b->dirpath, dlen);
            if (sep)
                child[dlen] = '/';
            memcpy(child + dlen + sep, name, nlen + 1);
            struct stat st;
            if (stat_path(child, &st) == 0 && S_ISDIR(st.st_mode))
                kind = WK_FS_PROVIDER_ENTRY_KIND_DIR;
            free(child);
        }
    }
    wkfuse_string_dup(&b->items[b->len].name, name);
    b->items[b->len].kind = kind;
    b->len++;
    return 0;
}

/* ---- per-op dispatch ---- */

static void do_getattr(const wkfuse_string_t *path,
                       wk_fs_provider_result_reply_data_error_t *out) {
    char *p = fuse_path(path);
    struct stat st;
    int r = p ? stat_path(p, &st) : -ENOMEM;
    free(p);
    if (r < 0) {
        reply_err(out, -r);
        return;
    }
    out->is_err = false;
    out->val.ok.tag = WK_FS_PROVIDER_REPLY_DATA_ATTR;
    out->val.ok.val.attr.kind = S_ISDIR(st.st_mode)
                                    ? WK_FS_PROVIDER_ENTRY_KIND_DIR
                                    : WK_FS_PROVIDER_ENTRY_KIND_FILE;
    out->val.ok.val.attr.size = (uint64_t)st.st_size;
}

static void do_readdir(const wkfuse_string_t *path,
                       wk_fs_provider_result_reply_data_error_t *out) {
    if (!g_ops.readdir) {
        reply_err(out, ENOSYS);
        return;
    }
    char *p = fuse_path(path);
    if (!p) {
        reply_err(out, ENOMEM);
        return;
    }
    struct fuse_file_info fi = {0};
    if (g_ops.opendir) {
        int r = g_ops.opendir(p, &fi);
        if (r < 0) {
            free(p);
            reply_err(out, -r);
            return;
        }
    }
    struct dirbuf b = {NULL, 0, 0, p};
    int r = g_ops.readdir(p, &b, dir_fill, 0, &fi, 0);
    if (g_ops.releasedir)
        g_ops.releasedir(p, &fi);
    free(p);
    if (r < 0) {
        for (size_t i = 0; i < b.len; i++)
            wkfuse_string_free(&b.items[i].name);
        free(b.items);
        reply_err(out, -r);
        return;
    }
    out->is_err = false;
    out->val.ok.tag = WK_FS_PROVIDER_REPLY_DATA_ENTRIES;
    out->val.ok.val.entries.ptr = b.items;
    out->val.ok.val.entries.len = b.len;
}

static void do_open(const wk_fs_provider_open_args_t *a,
                    wk_fs_provider_result_reply_data_error_t *out) {
    char *p = fuse_path(&a->path);
    if (!p) {
        reply_err(out, ENOMEM);
        return;
    }
    struct stat st;
    int exists = stat_path(p, &st) == 0;
    if (exists && a->exclusive) {
        free(p);
        reply_err(out, EEXIST);
        return;
    }
    struct fuse_file_info fi = {0};
    if (!exists) {
        if (!a->create) {
            free(p);
            reply_err(out, ENOENT);
            return;
        }
        if (!g_ops.create) {
            free(p);
            reply_err(out, EROFS);
            return;
        }
        fi.flags = O_RDWR | O_CREAT;
        int r = g_ops.create(p, 0666, &fi);
        if (r < 0) {
            free(p);
            reply_err(out, -r);
            return;
        }
        memset(&st, 0, sizeof(st));
        st.st_mode = S_IFREG;
    } else if (S_ISDIR(st.st_mode)) {
        uint64_t h = handle_alloc(p, 1, fi);
        free(p);
        if (h == 0) {
            reply_err(out, ENOMEM);
            return;
        }
        out->is_err = false;
        out->val.ok.tag = WK_FS_PROVIDER_REPLY_DATA_OPENED;
        out->val.ok.val.opened.handle = h;
        out->val.ok.val.opened.kind = WK_FS_PROVIDER_ENTRY_KIND_DIR;
        out->val.ok.val.opened.size = 0;
        return;
    } else {
        if (a->truncate) {
            if (!g_ops.truncate) {
                free(p);
                reply_err(out, EROFS);
                return;
            }
            int r = g_ops.truncate(p, 0, NULL);
            if (r < 0) {
                free(p);
                reply_err(out, -r);
                return;
            }
            st.st_size = 0;
        }
        /* Plain opens read; create/truncate imply write intent (see the
           header comment). Read-only daemons check exactly this. */
        fi.flags = a->truncate ? O_RDWR : O_RDONLY;
        if (g_ops.open) {
            int r = g_ops.open(p, &fi);
            if (r < 0) {
                free(p);
                reply_err(out, -r);
                return;
            }
        }
    }
    uint64_t h = handle_alloc(p, 0, fi);
    free(p);
    if (h == 0) {
        reply_err(out, ENOMEM);
        return;
    }
    out->is_err = false;
    out->val.ok.tag = WK_FS_PROVIDER_REPLY_DATA_OPENED;
    out->val.ok.val.opened.handle = h;
    out->val.ok.val.opened.kind = WK_FS_PROVIDER_ENTRY_KIND_FILE;
    out->val.ok.val.opened.size = (uint64_t)st.st_size;
}

static void do_read(const wk_fs_provider_read_args_t *a,
                    wk_fs_provider_result_reply_data_error_t *out) {
    struct handle *e = handle_get(a->handle);
    if (!e || e->is_dir) {
        reply_err(out, e ? EISDIR : ENOENT);
        return;
    }
    if (!g_ops.read) {
        reply_err(out, ENOSYS);
        return;
    }
    char *buf = malloc(a->len ? a->len : 1);
    if (!buf) {
        reply_err(out, ENOMEM);
        return;
    }
    int r = g_ops.read(e->path, buf, a->len, (off_t)a->offset, &e->fi);
    if (r < 0) {
        free(buf);
        reply_err(out, -r);
        return;
    }
    out->is_err = false;
    out->val.ok.tag = WK_FS_PROVIDER_REPLY_DATA_DATA;
    out->val.ok.val.data.bytes.ptr = (uint8_t *)buf;
    out->val.ok.val.data.bytes.len = (size_t)r;
    /* FUSE semantics: a short read from a regular file means end-of-file. */
    out->val.ok.val.data.eof = (uint32_t)r < a->len;
}

static void do_write(const wk_fs_provider_write_args_t *a,
                     wk_fs_provider_result_reply_data_error_t *out) {
    struct handle *e = handle_get(a->handle);
    if (!e || e->is_dir) {
        reply_err(out, e ? EISDIR : ENOENT);
        return;
    }
    if (!g_ops.write) {
        reply_err(out, EROFS);
        return;
    }
    int r = g_ops.write(e->path, (const char *)a->data.ptr, a->data.len,
                        (off_t)a->offset, &e->fi);
    if (r < 0) {
        reply_err(out, -r);
        return;
    }
    out->is_err = false;
    out->val.ok.tag = WK_FS_PROVIDER_REPLY_DATA_WRITTEN;
    out->val.ok.val.written = (uint64_t)r;
}

static void do_release(uint64_t h,
                       wk_fs_provider_result_reply_data_error_t *out) {
    struct handle *e = handle_get(h);
    if (e && !e->is_dir && g_ops.release)
        g_ops.release(e->path, &e->fi);
    handle_free(h);
    reply_done(out);
}

static void do_set_size(const wk_fs_provider_set_size_args_t *a,
                        wk_fs_provider_result_reply_data_error_t *out) {
    struct handle *e = handle_get(a->handle);
    if (!e || e->is_dir) {
        reply_err(out, e ? EISDIR : ENOENT);
        return;
    }
    if (!g_ops.truncate) {
        reply_err(out, EROFS);
        return;
    }
    int r = g_ops.truncate(e->path, (off_t)a->size, &e->fi);
    if (r < 0)
        reply_err(out, -r);
    else
        reply_done(out);
}

/* One path-arg mutation (mkdir/unlink/rmdir), sharing the shape. */
static void do_path_op(const wkfuse_string_t *path,
                       int (*mk)(const char *, mode_t),
                       int (*rm)(const char *),
                       wk_fs_provider_result_reply_data_error_t *out) {
    char *p = fuse_path(path);
    if (!p) {
        reply_err(out, ENOMEM);
        return;
    }
    int r;
    if (mk)
        r = mk(p, 0777);
    else if (rm)
        r = rm(p);
    else
        r = -EROFS;
    free(p);
    if (r < 0)
        reply_err(out, -r);
    else
        reply_done(out);
}

static void do_rename(const wk_fs_provider_rename_args_t *a,
                      wk_fs_provider_result_reply_data_error_t *out) {
    if (!g_ops.rename) {
        reply_err(out, EROFS);
        return;
    }
    char *src = fuse_path(&a->src);
    char *dst = fuse_path(&a->dest);
    int r = (src && dst) ? g_ops.rename(src, dst, 0) : -ENOMEM;
    free(src);
    free(dst);
    if (r < 0)
        reply_err(out, -r);
    else
        reply_done(out);
}

static void handle_op(const wk_fs_provider_op_t *op,
                      wk_fs_provider_result_reply_data_error_t *out) {
    switch (op->tag) {
    case WK_FS_PROVIDER_OP_GETATTR:
        do_getattr(&op->val.getattr, out);
        break;
    case WK_FS_PROVIDER_OP_READDIR:
        do_readdir(&op->val.readdir, out);
        break;
    case WK_FS_PROVIDER_OP_OPEN:
        do_open(&op->val.open, out);
        break;
    case WK_FS_PROVIDER_OP_READ:
        do_read(&op->val.read, out);
        break;
    case WK_FS_PROVIDER_OP_WRITE:
        do_write(&op->val.write, out);
        break;
    case WK_FS_PROVIDER_OP_RELEASE:
        do_release(op->val.release, out);
        break;
    case WK_FS_PROVIDER_OP_SET_SIZE:
        do_set_size(&op->val.set_size, out);
        break;
    case WK_FS_PROVIDER_OP_MKDIR:
        do_path_op(&op->val.mkdir, g_ops.mkdir, NULL, out);
        break;
    case WK_FS_PROVIDER_OP_UNLINK:
        do_path_op(&op->val.unlink, NULL, g_ops.unlink, out);
        break;
    case WK_FS_PROVIDER_OP_RMDIR:
        do_path_op(&op->val.rmdir, NULL, g_ops.rmdir, out);
        break;
    case WK_FS_PROVIDER_OP_RENAME:
        do_rename(&op->val.rename, out);
        break;
    default:
        reply_err(out, ENOSYS);
        break;
    }
}

int fuse_main_real(int argc, char *argv[], const struct fuse_operations *op,
                   size_t op_size, void *private_data) {
    (void)argc;
    (void)argv; /* the mountpoint is the consumer's wire, not our argv */

    /* Callers may be built against a larger fuse_operations than ours;
       copy what we know (fields are pointers — zero means "not set"). */
    memset(&g_ops, 0, sizeof(g_ops));
    memcpy(&g_ops, op, op_size < sizeof(g_ops) ? op_size : sizeof(g_ops));

    g_ctx.private_data = private_data;
    if (g_ops.init) {
        struct fuse_conn_info conn = {0};
        struct fuse_config cfg = {0};
        conn.proto_major = 7;
        conn.proto_minor = 38;
        conn.max_write = 128 * 1024;
        conn.max_read = 128 * 1024;
        g_ctx.private_data = g_ops.init(&conn, &cfg);
    }

    wk_fs_provider_request_t req;
    while (wk_fs_provider_next_request(&req)) {
        wk_fs_provider_result_reply_data_error_t out;
        handle_op(&req.op, &out);
        wk_fs_provider_reply(req.id, &out);
        wk_fs_provider_result_reply_data_error_free(&out);
        wk_fs_provider_request_free(&req);
    }

    if (g_ops.destroy)
        g_ops.destroy(g_ctx.private_data);
    return 0;
}

/* ---- POSIX stubs for daemons that reference what wasi has no kernel for
 * (see the declarations in fuse.h). ---- */

int lchown(const char *path, uid_t uid, gid_t gid) {
    (void)path;
    (void)uid;
    (void)gid;
    errno = ENOSYS;
    return -1;
}

int mkfifoat(int dirfd, const char *path, mode_t mode) {
    (void)dirfd;
    (void)path;
    (void)mode;
    errno = ENOSYS;
    return -1;
}

int mknodat(int dirfd, const char *path, mode_t mode, dev_t dev) {
    (void)dirfd;
    (void)path;
    (void)mode;
    (void)dev;
    errno = ENOSYS;
    return -1;
}

mode_t umask(mode_t mask) {
    (void)mask;
    return 0; /* no process umask on wasi; accepted and ignored */
}
