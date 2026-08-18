/* wk_compat.h — injected into every mupdf compile via XCFLAGS `-include`
 * (a make-var override, no upstream edit): declarations wasi-libc
 * deliberately omits ("WASI has no temp directories") that fitz references.
 * The matching definitions live in compat.c, linked into the viewer. */
#ifndef WK_MUPDF_COMPAT_H
#define WK_MUPDF_COMPAT_H

#ifdef __cplusplus
extern "C" {
#endif

/* fz_new_output_to_tempfile (source/fitz/document.c) — wasi-libc guards the
 * declaration out entirely. compat.c supplies a real implementation over
 * open(O_CREAT|O_EXCL). */
int mkstemp(char *template_);

#ifdef __cplusplus
}
#endif

#endif /* WK_MUPDF_COMPAT_H */
