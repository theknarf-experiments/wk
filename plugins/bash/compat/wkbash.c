/* Running external commands from bash, on a platform with no fork/exec.
 *
 * bash normally forks and then execve()s in the child. WASI has no fork, so
 * the patched execute_disk_command() calls wk_bash_run() instead: it runs the
 * program to completion through wk's `wk:exec` capability and reports the
 * status the shell would have waited for. That is exec semantics minus the
 * fork, which is exactly what the shell needs for a plain command.
 *
 * Command resolution. bash has already searched PATH and passes what it found
 * (`command`, possibly NULL) plus the name as typed (`typed`). Two cases:
 *
 *   1. PATH found a file — run it, with bash's argv unchanged.
 *   2. PATH found nothing — before giving up, consult /etc/wk-multicall, a
 *      plain "applet binary" table. wk's filesystem has no symlinks, which is
 *      how a multicall binary normally provides its hundred names, so the
 *      table takes their place: `ls /bin/coreutils.wasm` means "run that
 *      binary with argv[0] = ls". GNU coreutils (and busybox) dispatch on
 *      argv[0], so this is the same mechanism their symlink installs use.
 *
 * The child's output is captured, so it is written to bash's stdout/stderr
 * here. That means a command's output appears at the shell's own descriptors —
 * `>` redirection still cannot work, because saving and restoring a descriptor
 * needs dup(), which WASI does not have.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "wkexec.h"

#define MULTICALL_TABLE "/etc/wk-multicall"

/* Look `name` up in the multicall table. Returns a malloc'd binary path, or
 * NULL. Lines are "applet path"; blank lines and #comments are skipped. */
static char *multicall_lookup(const char *name) {
    FILE *f = fopen(MULTICALL_TABLE, "r");
    if (!f)
        return NULL;
    char line[512];
    char *found = NULL;
    while (!found && fgets(line, sizeof line, f)) {
        char *p = line;
        while (*p == ' ' || *p == '\t')
            p++;
        if (*p == '#' || *p == '\n' || *p == '\0')
            continue;
        char *applet = p;
        while (*p && *p != ' ' && *p != '\t')
            p++;
        if (!*p)
            continue;
        *p++ = '\0';
        while (*p == ' ' || *p == '\t')
            p++;
        char *bin = p;
        while (*p && *p != ' ' && *p != '\t' && *p != '\n')
            p++;
        *p = '\0';
        if (strcmp(applet, name) == 0 && *bin)
            found = strdup(bin);
    }
    fclose(f);
    return found;
}

static void write_all(int fd, const char *buf, size_t len) {
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, buf + off, len - off);
        if (n <= 0)
            break;
        off += (size_t)n;
    }
}

/* Run an external command. Returns its exit status, or -1 if it could not be
 * run at all (so the caller can fall through to bash's own error paths). */
int wk_bash_run(const char *command, char **argv, const char *typed) {
    char *resolved = NULL;
    const char *path = command;

    if (!path || !*path) {
        /* Not on PATH: try the multicall table, keyed by the name as typed. */
        resolved = multicall_lookup(typed ? typed : (argv ? argv[0] : ""));
        if (!resolved)
            return -1;
        path = resolved;
    }

    wk_result r;
    int rc = wk_run(path, (const char *const *)argv, NULL, 0, &r);
    if (rc != 0 || r.error) {
        /* Couldn't run it this way; if PATH had found something we still
         * report the failure, otherwise let bash say "command not found". */
        int had_path = (command && *command);
        if (had_path && r.error)
            fprintf(stderr, "%s: %s\n", typed ? typed : path, r.error);
        wk_result_free(&r);
        free(resolved);
        return had_path ? 126 : -1;
    }

    write_all(STDOUT_FILENO, r.stdout_data, r.stdout_len);
    write_all(STDERR_FILENO, r.stderr_data, r.stderr_len);
    int status = r.exit_code;
    wk_result_free(&r);
    free(resolved);
    return status;
}
