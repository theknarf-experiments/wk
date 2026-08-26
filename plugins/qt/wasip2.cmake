# CMake toolchain file for cross-building Qt 6.8.4 (and Qt apps) to
# wasm32-wasip2, for the wk runtime.
#
# Derived from wasi-sdk's own share/cmake/wasi-sdk-p2.cmake, but it is NOT a
# drop-in copy: this file additionally encodes every policy decision Qt's
# build makes on our behalf if we don't. Read the WHY comments before
# changing anything here — most of these lines cost an afternoon to discover.
#
# THE PLATFORM STRATEGY (the big one)
# -----------------------------------
# We build Qt as a genuine "WASI" platform, NOT as Qt's existing
# wasm-emscripten platform. So we deliberately LEAVE CMAKE_SYSTEM_NAME at the
# WASI value wasi-sdk's own toolchain sets, which makes Qt's
#   qt_set01(WASM CMAKE_SYSTEM_NAME STREQUAL "Emscripten" OR EMSCRIPTEN)
#   (qtbase/cmake/QtPlatformSupport.cmake:21)
# evaluate to 0. That is exactly what we want:
#   * we do NOT inherit ~4,000 lines of embind/emscripten::val code in
#     corelib+gui+network that wasi-sdk cannot compile,
#   * we do NOT inherit emcc-only link flags (-s FETCH=1, -s STACK_SIZE=5MB,
#     -s ALLOW_MEMORY_GROWTH) injected onto the global Platform target,
#   * we do NOT trip QtAutoDetectHelpers.cmake's "Can't find an Emscripten
#     SDK!" FATAL_ERROR,
#   * and the `minimal` and `offscreen` QPA plugins — which are excluded on
#     WASM — build for free, giving Widgets a testable platform plugin long
#     before the wk QPA exists.
#
# The one thing CMake's WASI platform module does NOT do is set UNIX=1
# (Emscripten-Initialize.cmake does; Platform/WASI.cmake is literally
# `set(WASI 1)` and nothing else). Qt gates its entire POSIX layer —
# qcore_unix.cpp, qfilesystemengine_unix.cpp, qeventdispatcher_unix.cpp,
# qtimerinfo_unix.cpp, qlocale_unix.cpp — on `CONDITION UNIX`, so without it
# QtCore has no platform layer at all and does not link.
#
# UNIX cannot be set here: CMake clears it after the toolchain file is loaded
# (verified — a probe project with this toolchain prints UNIX=[] WASI=[1]).
#
# The obvious fix is Qt's own Integrity idiom — qtbase/cmake/platforms/Platform/
# Integrity.cmake is a one-line `set(UNIX 1)`, and Qt PREPENDs cmake/platforms
# to CMAKE_MODULE_PATH before project() (QtAutoDetectHelpers.cmake:619). IT DOES
# NOT WORK FOR WASI, and the failure is silent. cmGlobalGenerator::EnableLanguage
# resolves `include(Platform/${CMAKE_SYSTEM_NAME})` against CMake's OWN Modules
# directory in preference to CMAKE_MODULE_PATH — which is what the comment right
# above that PREPEND ("CMake-provided platform modules take precedence") is
# telling you. Integrity works only because CMake ships no Platform/Integrity.
# CMake 4.4.2 DOES ship Modules/Platform/WASI.cmake, so a Qt-side copy is
# shadowed and never runs. Verified by probe: with ONLY qtbase/cmake/platforms
# on CMAKE_MODULE_PATH, a message() added to that file never prints and UNIX
# stays empty.
#
# So UNIX is granted instead by patches/qtbase-0001-wasi-platform.patch, in
# cmake/QtPlatformSupport.cmake next to the qt_set01(WASI ...) line — top-level
# directory scope, before add_subdirectory(src), reaching every module exactly
# the way WASM and ANDROID do. build-qtbase.sh's preflight checks for it.
#
# NOT SET HERE, ON PURPOSE
# ------------------------
#   CMAKE_SYSTEM_NAME  — wasi-sdk sets WASI; overriding it is the whole point.
#   UNIX               — cleared after this file; see above.
#   CMAKE_EXECUTABLE_SUFFIX — see the note further down.

set(CMAKE_SYSTEM_NAME WASI)
set(CMAKE_SYSTEM_VERSION 1)
set(CMAKE_SYSTEM_PROCESSOR wasm32)

set(triple wasm32-wasip2)

if(WIN32)
    set(WASI_HOST_EXE_SUFFIX ".exe")
else()
    set(WASI_HOST_EXE_SUFFIX "")
endif()

# WASI_SDK_PREFIX locates the SDK. wasi-sdk's own toolchain can default it from
# its own path; this file lives in the wk tree instead, so it must be told —
# by -DWASI_SDK_PREFIX=... (what build-qtbase.sh passes) or by the WASI_SDK
# environment variable that every build script in this repo already exports.
if(NOT WASI_SDK_PREFIX AND DEFINED ENV{WASI_SDK})
    set(WASI_SDK_PREFIX "$ENV{WASI_SDK}")
endif()
if(NOT WASI_SDK_PREFIX)
    message(FATAL_ERROR
        "wasip2.cmake: pass -DWASI_SDK_PREFIX=<wasi-sdk root> (or export "
        "WASI_SDK). This toolchain lives in the wk tree, not inside the SDK, "
        "so it cannot infer the SDK location from its own path.")
endif()

# try_compile() re-reads this toolchain file in a scratch project that inherits
# NO cache variables, so -DWASI_SDK_PREFIX would be empty there and the
# FATAL_ERROR above would fire during CMake's own compiler-ABI detection —
# reported, confusingly, as "CMAKE_C_COMPILER not set, after EnableLanguage".
# This is the supported way to forward a variable into those scratch projects,
# and Qt's configure runs a lot of them.
list(APPEND CMAKE_TRY_COMPILE_PLATFORM_VARIABLES WASI_SDK_PREFIX)

# Until every CMake we support ships Platform/WASI.cmake, keep wasi-sdk's copy
# (also just `set(WASI 1)`) reachable, so that WASI is at least defined on an
# older CMake. Note this has NO bearing on UNIX either way: see the long note at
# the top — CMake's builtin Modules/Platform wins over CMAKE_MODULE_PATH
# regardless of order, which is why UNIX is set from QtPlatformSupport.cmake.
list(APPEND CMAKE_MODULE_PATH "${WASI_SDK_PREFIX}/share/cmake")

set(CMAKE_C_COMPILER   ${WASI_SDK_PREFIX}/bin/clang${WASI_HOST_EXE_SUFFIX})
set(CMAKE_CXX_COMPILER ${WASI_SDK_PREFIX}/bin/clang++${WASI_HOST_EXE_SUFFIX})
set(CMAKE_ASM_COMPILER ${WASI_SDK_PREFIX}/bin/clang${WASI_HOST_EXE_SUFFIX})
set(CMAKE_AR           ${WASI_SDK_PREFIX}/bin/llvm-ar${WASI_HOST_EXE_SUFFIX})
set(CMAKE_RANLIB       ${WASI_SDK_PREFIX}/bin/llvm-ranlib${WASI_HOST_EXE_SUFFIX})
set(CMAKE_C_COMPILER_TARGET   ${triple})
set(CMAKE_CXX_COMPILER_TARGET ${triple})
set(CMAKE_ASM_COMPILER_TARGET ${triple})

# --- try_compile / config tests ---------------------------------------------
#
# EXECUTABLE, explicitly, and do NOT "optimise" this to STATIC_LIBRARY.
#
# The STATIC_LIBRARY trick is the usual advice for embedded cross builds where
# the toolchain cannot link a runnable binary. It is WRONG here, twice over:
#
#   1. wasi-sdk links fine. Verified: a C file using setjmp/longjmp and a C++
#      file using throw/catch both linked with the flags below and RAN under
#      `wasmtime run -W exceptions`.
#   2. Qt's architecture config test (qtbase/cmake/QtBaseConfigureTests.cmake)
#      builds a real executable and then reads magic strings back out of it
#      with file(STRINGS). With STATIC_LIBRARY there is no executable and
#      configure FATAL_ERRORs. Under a genuine WASI platform (WASM=0) Qt looks
#      for `architecture_test` with NO suffix, and that is exactly what this
#      toolchain produces.
#
# STATIC_LIBRARY would also make every link-only feature test (Qt has many)
# succeed spuriously, silently enabling features whose symbols do not exist in
# wasi-libc — the failure then surfaces 40 minutes later as an undefined
# symbol in a completely unrelated file.
set(CMAKE_TRY_COMPILE_TARGET_TYPE EXECUTABLE)

# CMAKE_EXECUTABLE_SUFFIX is deliberately left alone (wasi-sdk's own toolchain
# sets ".wasm"; CMake clears it again during compiler-information setup —
# verified by probe, the linked artifact is `t`, not `t.wasm`). Qt's arch test
# expects no suffix on a non-WASM platform, so leaving it empty is what makes
# that test pass. Do not "restore" the .wasm suffix here; rename the final app
# binary in the app's build.sh instead.

# --- exception handling / setjmp --------------------------------------------
#
# wk's host runtime is wasmtime with Config::wasm_exceptions ON, i.e. the
# exnref proposal. wasi-sdk emits the LEGACY EH encoding by default, which
# wasmtime REFUSES to load ("legacy_exceptions feature required for try
# instruction") — and it refuses at instantiate time, so a single translation
# unit compiled without these flags poisons the whole component and the error
# points nowhere near the offending file. Hence: global flags, every language,
# every object, no exceptions (pun intended).
#
#   -fwasm-exceptions            real C++ EH. QtCore, QtConcurrent and all of
#                                QtQml genuinely throw. It ALSO selects
#                                wasi-sdk 34's eh/ variant of libc++/libc++abi
#                                and puts lib/wasm32-wasip2/eh on the library
#                                search path — which is the only reason
#                                `-lunwind` below resolves. Note that
#                                `-fwasm-exceptions -fno-exceptions` still
#                                selects eh/, so Qt's own per-target
#                                -fno-exceptions cannot split the ABI. Passed
#                                for C too (clang accepts it and it is what
#                                keeps eh/ on the search path for C-only
#                                links, e.g. Qt's config tests).
#   -mllvm -wasm-enable-sjlj     real setjmp/longjmp. Qt's bundled libpng and
#                                libjpeg are setjmp-based error handling; on
#                                wasm clang errors out at codegen without
#                                this. Same recipe as plugins/mupdf.
#   -wasm-use-legacy-eh=false    emit exnref instead of the legacy try/catch.
#                                This is the flag that makes wasmtime accept
#                                the result. Non-obvious and non-optional.
#
# The four _WASI_EMULATED_* macros are needed at COMPILE time, not just link
# time: wasi-libc's <signal.h> and <sys/mman.h> are #error headers without
# them, so every Qt feature test that includes either fails misleadingly (and
# Qt then silently disables a feature you wanted).
set(WK_WASI_EH_FLAGS "-fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false")
set(WK_WASI_EMULATION_DEFINES "-D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID")

# *_INIT rather than plain *_FLAGS: these SEED the cache on first configure and
# stay overridable/appendable by the caller. Corollary, and it bites: changing
# them later has no effect on an existing build dir — delete build-target/ (or
# run build-qtbase.sh with WK_QT_RECONFIGURE=1) to pick up an edit.
set(CMAKE_C_FLAGS_INIT   "${WK_WASI_EMULATION_DEFINES} ${WK_WASI_EH_FLAGS}")
set(CMAKE_CXX_FLAGS_INIT "${WK_WASI_EMULATION_DEFINES} ${WK_WASI_EH_FLAGS}")
set(CMAKE_ASM_FLAGS_INIT "${WK_WASI_EMULATION_DEFINES} ${WK_WASI_EH_FLAGS}")

# Link-time companions:
#   -lunwind   MANDATORY with -fwasm-exceptions and easy to miss: without it
#              the link dies on `undefined symbol: _Unwind_RaiseException`.
#              It lives in lib/wasm32-wasip2/eh/, reachable only because
#              -fwasm-exceptions is also on the link line (CMake's link rule
#              passes <FLAGS>, so it is).
#   -lsetjmp   the runtime half of -wasm-enable-sjlj.
#   the four -lwasi-emulated-*  the runtime half of the four defines above.
#   -Wl,-z,stack-size=8388608   Qt's raster paint engine and QML's JS engine
#              recurse deeply; the default 64KB shadow stack is nowhere near
#              enough, and overflowing it presents as a mystery trap rather
#              than a stack-overflow message. plugins/mupdf needs the same 8MB.
set(CMAKE_EXE_LINKER_FLAGS_INIT
    "-lunwind -lsetjmp -lwasi-emulated-signal -lwasi-emulated-mman -lwasi-emulated-process-clocks -lwasi-emulated-getpid -Wl,-z,stack-size=8388608")

# No LTO: it re-runs codegen at link time, where the -mllvm EH flags above are
# not in effect, and the result is a component wasmtime rejects.
set(CMAKE_INTERPROCEDURAL_OPTIMIZATION OFF)

# --- static only ------------------------------------------------------------
#
# wasm has no dlopen and no shared objects. Every Qt module, every QPA plugin
# and every image-format plugin is linked into the app binary, and Qt's
# CMake generates the Q_IMPORT_PLUGIN glue for that case itself
# (QtPublicPluginHelpers.cmake). PIC is meaningless here and only costs code
# size.
set(BUILD_SHARED_LIBS OFF CACHE BOOL "wasm has no shared libraries")
set(CMAKE_POSITION_INDEPENDENT_CODE OFF)

# --- find() root behaviour --------------------------------------------------
#
# NEVER for PROGRAM: build-time tools (moc, rcc, uic, qmlcachegen, python,
# perl, ninja) must come from the HOST, not from the wasm sysroot. Qt's cross
# build would otherwise try to run a .wasm as moc.
# ONLY for the rest: a stray /opt/homebrew/include/png.h or
# /usr/local/lib/libz.dylib found by a Qt system_* feature test is the classic
# way a cross build produces an unlinkable target. Everything Qt needs
# (freetype, harfbuzz, libpng, libjpeg, zlib, pcre2, md4c,
# double-conversion) is bundled in qtbase/src/3rdparty anyway, and
# build-qtbase.sh forces every FEATURE_system_* off to match.
# list(APPEND ...) and not set(): this reads whatever the caller already put in
# CMAKE_FIND_ROOT_PATH (including a -DCMAKE_FIND_ROOT_PATH=... cache entry) and
# adds the wasi sysroot to it. `set()` would define a NORMAL variable that
# silently shadows the caller's cache entry — which matters because an APP
# building against this Qt must add plugins/qt/sysroot to the find root, and
# with a plain set() here its -D is ignored and find_package(Qt6) fails with
# "Could not find a package configuration file provided by Qt6" while
# Qt6Config.cmake is sitting right there. So:
#
#   cmake -DCMAKE_TOOLCHAIN_FILE=.../wasip2.cmake \
#         -DCMAKE_FIND_ROOT_PATH=.../plugins/qt/sysroot \
#         -DCMAKE_PREFIX_PATH=.../plugins/qt/sysroot ...
list(APPEND CMAKE_FIND_ROOT_PATH "${WASI_SDK_PREFIX}/share/wasi-sysroot")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)

# pkg-config must never run: it would answer from the host's .pc files.
# Qt has its own guard for this (QtBuildRepoHelpers.cmake skips pkg-config on
# APPLE/WIN32/QNX/ANDROID/WASM, and patches/ adds WASI to that list), but belt
# and braces — an unset executable is unambiguous.
set(PKG_CONFIG_EXECUTABLE "" CACHE FILEPATH "no pkg-config when cross-compiling to wasm")
