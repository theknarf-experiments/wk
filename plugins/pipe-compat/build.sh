#!/usr/bin/env bash
# A real pipe() for wk guests, on wasip2.
#
# wasi-libc's wasip2 pipe() is ENOSYS — the component model had nothing to
# build one from until wasip3's async streams. wk has a pipe (wk:exec's), and
# wasip2 keeps the descriptor table in guest memory behind a vtable, so this
# shim puts wk's pipe behind a file descriptor. After that it is libc's read,
# write, close, dup, poll and fcntl doing the work, for any guest that links
# this — not just a patched shell.
#
# Link the objects this produces (pipe.o + the wk:exec bindings) into a
# wasm32-wasip2 component, exactly as plugins/tty-compat is linked for termios.
set -euo pipefail
cd "$(dirname "$0")"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
EXEC="$(pwd)/../exec-compat"
EXECGEN="$EXEC/gen"

# The descriptor table's layout is private to wasi-libc and is transcribed in
# wasilibc_descriptor_table.h from one exact revision (see that file's header).
# A different toolchain may move a field, which would not fail to link — it
# would silently corrupt. So check, and say why.
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *)
        echo "pipe-compat: expected $EXPECT, got: $WASI_SDK" >&2
        echo "  wasilibc_descriptor_table.h transcribes wasi-libc's private" >&2
        echo "  descriptor table layout for that release. Re-check it against" >&2
        echo "  the new wasi-libc before changing this." >&2
        exit 1
        ;;
esac

mkdir -p "$EXECGEN"
wit-bindgen c --world exec-host "$EXEC/wit" --out-dir "$EXECGEN"

"$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 \
    -I. -I"$EXEC" -I"$EXECGEN" \
    -c pipe.c -o pipe.o

# A self-test that is just a C program using pipe(2): write, read, fstat, dup,
# and EOF after the last writer closes. Nothing in it knows wk exists, which is
# the point of the exercise.
"$WASI_SDK/bin/clang" --target=wasm32-wasip2 -O2 \
    -I. -I"$EXEC" -I"$EXECGEN" \
    selftest.c pipe.o "$EXECGEN/exec_host.c" "$EXECGEN/exec_host_component_type.o" \
    -o pipetest.wasm

echo "built plugins/pipe-compat/pipe.o (link it with the wk:exec bindings)"
echo "  and pipetest.wasm — run it with:"
echo "  wk images build plugins/pipe-compat/Dockerfile --tag pipe-test"
