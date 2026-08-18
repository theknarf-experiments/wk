/* zipfs: a read-only FUSE filesystem presenting the contents of a zip
 * archive, built on miniz (unmodified upstream, fetched at build). As a wk
 * node: wire a Volume/BindMount holding the .zip into this node, wire this
 * node into an app, and the archive's tree appears at the app's mount —
 * drop-an-archive-on-the-canvas, browse it anywhere.
 *
 * The archive is found in this daemon's own filesystem: the first argv path
 * that exists, else the first `*.zip` at the root. Indexing is LAZY — the
 * first filesystem request triggers the scan — so the archive may be wired
 * in after the node starts; until one appears every lookup is ENOENT.
 *
 * Reads decompress a whole member on first touch and cache the most recent
 * one (zip members are deflate streams; there is no random access into
 * them), so sequential consumer reads of one file cost one extraction.
 */

#define FUSE_USE_VERSION 31

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <fuse.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "miniz.h"

/* ---- the archive index: every member path, normalized, dirs included ---- */

struct entry {
    char *path; /* "a/b/c", no leading or trailing slash */
    int is_dir;
    mz_uint file_index;  /* miniz index (files only) */
    mz_uint64 size;      /* uncompressed size (files only) */
};

static struct {
    mz_zip_archive zip;
    int state; /* 0 = not tried, 1 = open, -1 = archive unusable */
    struct entry *entries;
    size_t len, cap;
    /* Most-recently-extracted member (see the header comment). */
    mz_uint cached_index;
    char *cached_data;
    size_t cached_len;
} g;

static const char *g_argv_path; /* archive path from argv, if any */

static struct entry *find_entry(const char *path) {
    for (size_t i = 0; i < g.len; i++)
        if (strcmp(g.entries[i].path, path) == 0)
            return &g.entries[i];
    return NULL;
}

static void push_entry(const char *path, int is_dir, mz_uint file_index,
                       mz_uint64 size) {
    if (path[0] == '\0' || find_entry(path))
        return;
    if (g.len == g.cap) {
        g.cap = g.cap ? g.cap * 2 : 64;
        g.entries = realloc(g.entries, g.cap * sizeof(*g.entries));
    }
    g.entries[g.len].path = strdup(path);
    g.entries[g.len].is_dir = is_dir;
    g.entries[g.len].file_index = file_index;
    g.entries[g.len].size = size;
    g.len++;
}

/* Record `path`'s parent directories (zip files routinely omit dir members). */
static void push_parents(const char *path) {
    char buf[1024];
    snprintf(buf, sizeof(buf), "%s", path);
    for (char *slash = strrchr(buf, '/'); slash; slash = strrchr(buf, '/')) {
        *slash = '\0';
        push_entry(buf, 1, 0, 0);
    }
}

/* The archive to serve: the argv path if it exists, else the first *.zip in
 * this node's root (where a wired Volume/BindMount lands by default). */
static char *locate_archive(void) {
    if (g_argv_path) {
        FILE *f = fopen(g_argv_path, "rb");
        if (f) {
            fclose(f);
            return strdup(g_argv_path);
        }
    }
    DIR *d = opendir("/");
    if (!d)
        return NULL;
    struct dirent *de;
    char *found = NULL;
    while (!found && (de = readdir(d)) != NULL) {
        size_t n = strlen(de->d_name);
        if (n > 4 && strcmp(de->d_name + n - 4, ".zip") == 0) {
            found = malloc(n + 2);
            snprintf(found, n + 2, "/%s", de->d_name);
        }
    }
    closedir(d);
    return found;
}

/* Open + index the archive on first use. Returns 1 when an index is ready.
 * A missing archive stays retryable (it may be wired in later); a corrupt
 * one is remembered as unusable. */
static int ensure_index(void) {
    if (g.state == 1)
        return 1;
    if (g.state == -1)
        return 0;
    char *path = locate_archive();
    if (!path)
        return 0; /* nothing to serve yet — retry on the next request */
    memset(&g.zip, 0, sizeof(g.zip));
    if (!mz_zip_reader_init_file(&g.zip, path, 0)) {
        fprintf(stderr, "zipfs: %s is not a readable zip archive\n", path);
        free(path);
        g.state = -1;
        return 0;
    }
    mz_uint n = mz_zip_reader_get_num_files(&g.zip);
    for (mz_uint i = 0; i < n; i++) {
        mz_zip_archive_file_stat st;
        if (!mz_zip_reader_file_stat(&g.zip, i, &st))
            continue;
        /* Normalize: strip leading "./" and any trailing slash. */
        const char *p = st.m_filename;
        while (p[0] == '.' && p[1] == '/')
            p += 2;
        char buf[1024];
        snprintf(buf, sizeof(buf), "%s", p);
        size_t len = strlen(buf);
        int is_dir = mz_zip_reader_is_file_a_directory(&g.zip, i);
        if (len > 0 && buf[len - 1] == '/') {
            buf[len - 1] = '\0';
            is_dir = 1;
        }
        push_entry(buf, is_dir, i, st.m_uncomp_size);
        push_parents(buf);
    }
    fprintf(stderr, "zipfs: serving %s (%u members)\n", path, (unsigned)n);
    free(path);
    g.state = 1;
    return 1;
}

/* Whole-member extraction with a one-slot cache. */
static const char *member_data(struct entry *e) {
    if (g.cached_data && g.cached_index == e->file_index)
        return g.cached_data;
    char *data = malloc(e->size ? e->size : 1);
    if (!data)
        return NULL;
    if (!mz_zip_reader_extract_to_mem(&g.zip, e->file_index, data, e->size,
                                      0)) {
        free(data);
        return NULL;
    }
    free(g.cached_data);
    g.cached_data = data;
    g.cached_len = e->size;
    g.cached_index = e->file_index;
    return data;
}

/* ---- FUSE callbacks ---- */

static int zipfs_getattr(const char *path, struct stat *st,
                         struct fuse_file_info *fi) {
    (void)fi;
    memset(st, 0, sizeof(*st));
    if (strcmp(path, "/") == 0) {
        st->st_mode = S_IFDIR | 0555;
        return 0;
    }
    if (!ensure_index())
        return -ENOENT;
    struct entry *e = find_entry(path + 1);
    if (!e)
        return -ENOENT;
    st->st_mode = e->is_dir ? (S_IFDIR | 0555) : (S_IFREG | 0444);
    st->st_size = (off_t)e->size;
    return 0;
}

static int zipfs_readdir(const char *path, void *buf, fuse_fill_dir_t filler,
                         off_t offset, struct fuse_file_info *fi,
                         enum fuse_readdir_flags flags) {
    (void)offset;
    (void)fi;
    (void)flags;
    if (!ensure_index())
        return strcmp(path, "/") == 0 ? 0 : -ENOENT;
    const char *prefix = path + 1; /* "" for the root */
    size_t plen = strlen(prefix);
    if (plen > 0) {
        struct entry *e = find_entry(prefix);
        if (!e)
            return -ENOENT;
        if (!e->is_dir)
            return -ENOTDIR;
    }
    filler(buf, ".", NULL, 0, 0);
    filler(buf, "..", NULL, 0, 0);
    for (size_t i = 0; i < g.len; i++) {
        const char *p = g.entries[i].path;
        if (plen > 0) {
            if (strncmp(p, prefix, plen) != 0 || p[plen] != '/')
                continue;
            p += plen + 1;
        }
        if (strchr(p, '/'))
            continue; /* deeper than one level */
        struct stat st = {0};
        st.st_mode = g.entries[i].is_dir ? (S_IFDIR | 0555) : (S_IFREG | 0444);
        st.st_size = (off_t)g.entries[i].size;
        filler(buf, p, &st, 0, 0);
    }
    return 0;
}

static int zipfs_open(const char *path, struct fuse_file_info *fi) {
    if (!ensure_index())
        return -ENOENT;
    struct entry *e = find_entry(path + 1);
    if (!e)
        return -ENOENT;
    if (e->is_dir)
        return -EISDIR;
    if ((fi->flags & O_ACCMODE) != O_RDONLY)
        return -EROFS;
    fi->fh = (uint64_t)(e - g.entries);
    return 0;
}

static int zipfs_read(const char *path, char *buf, size_t size, off_t offset,
                      struct fuse_file_info *fi) {
    (void)path;
    if (!ensure_index())
        return -ENOENT;
    struct entry *e = &g.entries[fi->fh];
    const char *data = member_data(e);
    if (!data)
        return -EIO;
    if ((mz_uint64)offset >= e->size)
        return 0;
    if (offset + size > e->size)
        size = e->size - offset;
    memcpy(buf, data + offset, size);
    return (int)size;
}

static const struct fuse_operations zipfs_oper = {
    .getattr = zipfs_getattr,
    .readdir = zipfs_readdir,
    .open = zipfs_open,
    .read = zipfs_read,
};

int main(int argc, char *argv[]) {
    if (argc > 1 && argv[1][0] != '-')
        g_argv_path = argv[1];
    return fuse_main(argc, argv, &zipfs_oper, NULL);
}
