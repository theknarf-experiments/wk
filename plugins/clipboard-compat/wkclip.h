/* wkclip.h — a small C API over wk's wk:clipboard interface.
 *
 * The clipboard sibling of ../midi-compat (wkmidi.h) and ../audio-compat
 * (wkaudio.h): a C or C++ program compiled against this header reads and
 * writes the HOST's system clipboard without touching wit-bindgen output.
 * Link wkclip.c plus the bindings a consumer's build.sh regenerates into gen/
 * (plugins/qt/build-qpa.sh is the worked example):
 *
 *     wit-bindgen c --world wkclipboard ../clipboard-compat/wit \
 *         --out-dir ../clipboard-compat/gen
 *
 * ...and, in the final link of the APP (not an archive member — a static
 * archive member nothing references gets dropped, and this object exists
 * purely for its `component-type` custom section):
 *
 *     gen/wkclipboard_component_type.o
 *
 * THIS IS A GRANTED CAPABILITY, not an ambient one. Everything here returns
 * "nothing there" unless the node is wired to a Clipboard node on wk's canvas
 * AND its capability token still allows the action — and `read` and `write`
 * are separately grantable, so a node can be allowed to copy out without
 * being allowed to see what the user copied elsewhere. Write code that copes
 * with a permanent no: wkclip_get() returning 0 forever is a normal state,
 * not an error to report, and wkclip_set() silently doing nothing is too.
 *
 * Model (poll-based, like wk:midi — no pollable, because the host platform
 * APIs underneath have no change notification to hang one on):
 *
 *     char *text; uint64_t seq;
 *     if (wkclip_get(&text, &seq)) {           // 1 = a snapshot came back
 *         paste(text);
 *         free(text);                          // caller owns it
 *     }
 *     wkclip_set("copied from a wk node");
 *
 * `seq` increments only when the host sees the text actually change, so
 * remembering the seq your own wkclip_set() produced is how you tell "still
 * mine" from "somebody else copied something" — see plugins/qt/qpa's
 * QWkClipboard::ownsMode(), which is the reason the field exists.
 */
#ifndef WKCLIP_SHIM_H
#define WKCLIP_SHIM_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Read the host clipboard.
 *
 * On success returns 1, stores a freshly malloc'd NUL-terminated UTF-8 string
 * in *out_text (the CALLER frees it) and the snapshot's sequence number in
 * *out_seq if it is non-NULL. Returns 0 and touches nothing when there is no
 * snapshot to read: not wired, token denies `clipboard`/`read`, the host has
 * no clipboard, or it holds something that is not text. Those are deliberately
 * indistinguishable — a sandboxed node must not be able to probe for a
 * clipboard it may not see.
 *
 * The returned text may contain embedded NULs (it is a WIT `string`, i.e. a
 * length-delimited byte range); the NUL terminator is added for C's benefit
 * and *out_seq is the only reliable change signal. Cheap enough to call from
 * a synchronous paste handler — the host publishes a value, this does not
 * block on the window system. */
int wkclip_get(char **out_text, uint64_t *out_seq);

/* Put a NUL-terminated UTF-8 string on the host clipboard.
 *
 * No return value on purpose: a denied write is silently dropped, exactly
 * like wk:midi's send. A NULL argument is a no-op. */
void wkclip_set(const char *text);

#ifdef __cplusplus
}
#endif

#endif /* WKCLIP_SHIM_H */
