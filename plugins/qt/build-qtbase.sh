#!/usr/bin/env bash
# Cross-build qtbase 6.8.4 for wasm32-wasip2 into plugins/qt/sysroot.
#
# The shape of this build, and why each half exists:
#
#   ./wasip2.cmake     the toolchain: wasi-sdk clang, the exnref-EH + sjlj flag
#                      set, static-only, find-root modes. Read its header — it
#                      is where the platform strategy is argued.
#   ./host             a native Qt 6.8.4 of the SAME version, built by
#                      ./build-host.sh, supplying moc/rcc/uic/syncqt. Qt
#                      FATAL_ERRORs without QT_HOST_PATH.
#   ./patches          the qtbase source/CMake changes this port needs. Applied
#                      here, idempotently. The preflight below refuses to
#                      configure until the two structural ones are in place,
#                      because their absence produces error messages that send
#                      you a long way in the wrong direction.
#   ./sysroot          CMAKE_INSTALL_PREFIX — the cross Qt an app build later
#                      points CMAKE_PREFIX_PATH at.
#
# WE ARE NOT BUILDING QT'S wasm-emscripten PLATFORM. CMAKE_SYSTEM_NAME stays
# WASI, so Qt's WASM=0 and we compile the generic UNIX corelib. The trade is
# explicit: we inherit ~4,000 lines of embind code NOT AT ALL, and in exchange
# we must plug the handful of wasi-libc gaps the UNIX backends assume
# (sigaction, getpwuid/getgrgid, sys/wait.h, eventfd). That is what patches/ is
# for, and it is a dozen one-line guards rather than a rewrite.
#
# THE FEATURE FLAGS BELOW ARE NOT DECORATION. Under a genuine WASI platform a
# pile of features that Qt's own Emscripten build gets for free (because they
# are written `AUTODETECT NOT WASM` / `CONDITION NOT WASM`) autodetect back ON
# for us. FEATURE_thread is the dangerous one: wasi-libc DEFINES pthread_create
# as a stub that returns ENOTSUP, so the config test PASSES, everything links,
# and threads silently never run. Turning it off is not a workaround — it is
# exactly the configuration Qt ships for Emscripten.
#
# wasm-opt: clang runs it as an optional post-link pass and the wasm-opt on
# PATH cannot parse exnref; it would corrupt the output. Every cmake/ninja
# invocation here therefore runs under a PATH that omits it. (There IS one on
# this machine: ~/.cargo/bin/wasm-opt.) Same trap plugins/mupdf documents.
#
# Idempotent: reconfigures only when there is no cache (or WK_QT_RECONFIGURE=1),
# and ninja does the rest. Long: budget an hour-plus, and run it in the
# background rather than under a 10-minute foreground timeout.
#
# Knobs: WK_QT_RECONFIGURE=1 (force a fresh configure)
#        WK_QT_STAGES="Core"  (stop after QtCore; default is Core Gui Widgets all)
#        JOBS=N  QT_HOST_PATH=...  LOGDIR=...
set -euo pipefail
cd "$(dirname "$0")"

# --- toolchain guard (same as plugins/mupdf/build.sh) -----------------------
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *)
        echo "qt/build-qtbase: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2
        exit 1
        ;;
esac

QT_VER=6.8.4
QT_SERIES=6.8
SRCDIR="$PWD/src"
TARBALLS="$PWD/tarballs"
PATCHDIR="$PWD/patches"
QTBASE_SRC="$SRCDIR/qtbase-everywhere-src-$QT_VER"
BUILD="$PWD/build-target/qtbase"
SYSROOT="$PWD/sysroot"
HOST_PREFIX="${QT_HOST_PATH:-$PWD/host}"

# ../resolv-compat's sysroot, built on demand: Qt's FindWrapResolv probe needs
# both libresolv.a and resolv.h to exist before configure runs, and a missing
# one shows up as FEATURE_libresolv=ON being "an invalid feature" rather than
# as anything mentioning DNS.
RESOLV_SYSROOT="$PWD/../resolv-compat/sysroot"
if [ ! -f "$RESOLV_SYSROOT/lib/libresolv.a" ]; then
    echo "=== building ../resolv-compat (libresolv.a for QDnsLookup)"
    (cd "$PWD/../resolv-compat" && WASI_SDK="$WASI_SDK" ./build.sh)
fi
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
mkdir -p "$SRCDIR" "$TARBALLS" "$LOGDIR" "$(dirname "$BUILD")"

# The mkspec directory name. It must EXIST under qtbase/mkspecs or
# QtMkspecHelpers.cmake:122 FATAL_ERRORs, and it doubles as a real include
# directory (QtPlatformTargetHelpers.cmake:76-95) so that Qt's
# `#include "qplatformdefs.h"` resolves out of it. patches/ creates it.
MKSPEC=wasi-clang-wasip2

# clang runs wasm-opt from PATH; keep it out of reach. cmake and ninja live in
# /opt/homebrew/bin here, so that must stay in.
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

# --- upstream, fetched not vendored -----------------------------------------
# The tarball is "-everywhere-opensource-src-" upstream but extracts to
# "-everywhere-src-". Not a typo; verified against download.qt.io.
if [ ! -d "$QTBASE_SRC" ]; then
    tar_path="$TARBALLS/qtbase-everywhere-opensource-src-$QT_VER.tar.xz"
    if [ ! -f "$tar_path" ]; then
        echo "fetching qtbase $QT_VER..."
        curl -fsSL --retry 3 -o "$tar_path.part" \
            "https://download.qt.io/archive/qt/$QT_SERIES/$QT_VER/submodules/$(basename "$tar_path")"
        mv "$tar_path.part" "$tar_path"
    fi
    echo "extracting qtbase $QT_VER..."
    tar xJf "$tar_path" -C "$SRCDIR"
fi

# --- patches ----------------------------------------------------------------
# patches/qtbase-NNNN-*.patch, applied -p1 at the qtbase source root. Every
# patch must be reverse-checkable so this stays re-runnable. See
# patches/README.md for the convention and the ledger.
if [ -d "$PATCHDIR" ]; then
    for p in "$PATCHDIR"/qtbase-*.patch; do
        [ -e "$p" ] || continue
        if git -C "$QTBASE_SRC" apply --reverse --check "$p" >/dev/null 2>&1; then
            echo "  patch (already applied): $(basename "$p")"
            continue
        fi
        echo "  patch: $(basename "$p")"
        git -C "$QTBASE_SRC" apply "$p"
    done
fi

# --- preflight --------------------------------------------------------------
# Two structural things must exist before configure has any chance. Checking
# them here turns two famously misleading failures into one clear sentence.
fail=0
if ! grep -q 'set(UNIX 1)' "$QTBASE_SRC/cmake/QtPlatformSupport.cmake" 2>/dev/null; then
    cat >&2 <<'EOF'
qt/build-qtbase: qtbase/cmake/QtPlatformSupport.cmake does not set UNIX for WASI

  CMake's own Platform/WASI.cmake is a comment and WASI-Initialize.cmake is a
  bare `set(WASI 1)` -- unlike Emscripten-Initialize.cmake, NEITHER sets UNIX.
  Qt gates its whole POSIX layer (qcore_unix.cpp, qfilesystemengine_unix.cpp,
  qeventdispatcher_unix.cpp, qtimerinfo_unix.cpp, qlocale_unix.cpp,
  qstandardpaths_unix.cpp) on `CONDITION UNIX`. Without it those sources are
  never added, syncqt never copies qcore_unix_p.h, and the build dies on
  "private/qcore_unix_p.h file not found" in a dozen unrelated files.

  Do NOT try to fix this with a cmake/platforms/Platform/WASI.cmake the way
  Integrity does. That idiom only works for a platform CMake does not already
  know: EnableLanguage resolves `include(Platform/<name>)` against CMake's OWN
  Modules dir in preference to CMAKE_MODULE_PATH, and CMake 4.4.2 ships
  Modules/Platform/WASI.cmake -- so a Qt-side copy is silently shadowed and
  never runs. UNIX is set from QtPlatformSupport.cmake instead, next to
  qt_set01(WASI ...). Apply patches/qtbase-0001-wasi-platform.patch.
EOF
    fail=1
fi
if [ ! -d "$QTBASE_SRC/mkspecs/$MKSPEC" ]; then
    cat >&2 <<EOF
qt/build-qtbase: missing qtbase/mkspecs/$MKSPEC

  QtMkspecHelpers.cmake:122 FATAL_ERRORs on an unknown mkspec, and the
  directory doubles as an include path so that Qt's #include "qplatformdefs.h"
  resolves. mkspecs/common/wasm/qplatformdefs.h cannot be reused: it includes
  grp.h, pwd.h, sys/ipc.h, sys/wait.h and net/if.h (none of which exist in the
  wasi sysroot) and uses O_LARGEFILE (undeclared on wasi-libc).

  Add it via patches/qtbase-0001-wasi-platform.patch.
EOF
    fail=1
fi
# moc proves the host tree installed. Look in BOTH libexec/ and bin/: on this
# build (Qt 6.8.4, macOS, INSTALL_LIBEXECDIR defaulting to libexec) moc, rcc,
# uic, syncqt, qmlcachegen and qmltyperegistrar all land in libexec/, and only
# the user-facing tools (qmake, qtpaths, qml, qmllint) land in bin/. Other Qt
# configurations put them in bin/, so accept either rather than pinning one.
if [ ! -x "$HOST_PREFIX/libexec/moc" ] && [ ! -x "$HOST_PREFIX/bin/moc" ]; then
    echo "qt/build-qtbase: no host Qt at $HOST_PREFIX -- run ./build-host.sh first" >&2
    echo "  (QtBuildHelpers.cmake:351 FATAL_ERRORs without a valid QT_HOST_PATH)" >&2
    fail=1
fi
[ "$fail" = 0 ] || exit 1

# --- the configure command line ---------------------------------------------
# Assembled as an array so every flag can carry the reason it is here. Feature
# names are normalised the way Qt does it (every non-alphanumeric becomes an
# underscore: "raster-64bit" -> FEATURE_raster_64bit). Forcing a feature OFF is
# always safe; forcing one ON whose CONDITION is false is a configure error.
CFG=(
    -G Ninja -S "$QTBASE_SRC" -B "$BUILD"
    -DCMAKE_TOOLCHAIN_FILE="$PWD/wasip2.cmake"
    -DWASI_SDK_PREFIX="$WASI_SDK"
    -DQT_QMAKE_TARGET_MKSPEC="$MKSPEC"
    -DQT_HOST_PATH="$HOST_PREFIX"
    -DCMAKE_INSTALL_PREFIX="$SYSROOT"
    # ../resolv-compat's libresolv.a + resolv.h, for QDnsLookup (see
    # FEATURE_libresolv below). wasip2.cmake sets FIND_ROOT_PATH_MODE_LIBRARY /
    # _INCLUDE to ONLY, so a path has to be on CMAKE_FIND_ROOT_PATH to be
    # searched at all -- appending here rather than replacing keeps the
    # wasi-sysroot entry the toolchain file put there.
    -DCMAKE_FIND_ROOT_PATH="$WASI_SDK/share/wasi-sysroot;$RESOLV_SYSROOT"
    # ...and the header separately, as a -I: Qt's FindWrapResolv probes with a
    # compile test that gets no include dirs from the find root. See
    # WK_EXTRA_INCLUDE_DIRS in wasip2.cmake.
    -DWK_EXTRA_INCLUDE_DIRS="$RESOLV_SYSROOT/include"
    -DCMAKE_BUILD_TYPE=Release
    -DBUILD_SHARED_LIBS=OFF
    -DQT_BUILD_EXAMPLES=OFF
    -DQT_BUILD_TESTS=OFF
    -DQT_BUILD_BENCHMARKS=OFF
    -DQT_BUILD_MANUAL_TESTS=OFF

    # wasi-sdk 34-rc.2's clang reports itself as Clang 23. Qt 6.8.4 predates it
    # and trips new diagnostics that Qt's developer defaults promote to errors.
    -DWARNINGS_ARE_ERRORS=OFF

    # M0/M1 QPA: offscreen and minimal build for free under WASI
    # (src/plugins/platforms/CMakeLists.txt:7-11 excludes them only on ANDROID
    # and WASM), which gives Widgets a testable platform plugin before the wk
    # QPA exists. Flip QT_QPA_DEFAULT_PLATFORM to wk at M2.
    -DQT_QPA_PLATFORMS="offscreen;minimal"
    -DQT_QPA_DEFAULT_PLATFORM=offscreen

    # THREADS. The single most important flag here. configure.cmake:1048 is
    # `AUTODETECT NOT WASM` -- we are not WASM, so it would come back ON,
    # compile qthread_unix.cpp against wasi-libc's pthread STUBS
    # (pthread_create is a two-instruction body returning ENOTSUP), link
    # cleanly, and hang at runtime. thread=OFF is a first-class Qt
    # configuration, not a hack: it is what Qt ships for Emscripten.
    -DFEATURE_thread=OFF
    -DFEATURE_future=OFF
    -DFEATURE_concurrent=OFF
    # cxx11_future's config test WRONGLY passes: wasi-sdk 34's libc++
    # __config_site declares _LIBCPP_HAS_THREADS 1 for wasip2 even though the
    # pthreads under it are ENOSYS stubs. Force it.
    -DFEATURE_cxx11_future=OFF

    # No fork/exec/wait on wasip2. NB processENVIRONMENT, not process:
    # qprocess.cpp and qprocess_unix.cpp are gated on processenvironment
    # (src/corelib/CMakeLists.txt:1011-1024), and those need sys/wait.h.
    -DFEATURE_process=OFF
    -DFEATURE_processenvironment=OFF
    -DFEATURE_forkfd_pidfd=OFF
    -DFEATURE_multiprocess=OFF

    # No dlopen. wasi-libc SHIPS dlfcn.h and a libdl.a whose dlopen is a stub
    # over a static error string, so the feature would come back ON (its
    # CONDITION is bare UNIX, with no autodetect escape) and drag in QLibrary
    # plus the ELF parser. Static plugin registration survives this:
    # QPluginLoader::staticPlugins() sits outside the QT_CONFIG(library) guards
    # in qfactoryloader.cpp.
    -DFEATURE_dlopen=OFF
    -DFEATURE_dladdr=OFF
    -DFEATURE_library=OFF

    # No SysV/POSIX IPC: no sys/shm.h, sys/sem.h, shm_open or sem_init. These
    # would auto-fall on their own config tests; pinning them removes a
    # dependency on config-test behaviour under cross-compilation.
    -DFEATURE_sharedmemory=OFF
    -DFEATURE_systemsemaphore=OFF
    -DFEATURE_posix_shm=OFF
    -DFEATURE_posix_sem=OFF
    -DFEATURE_sysv_shm=OFF
    -DFEATURE_sysv_sem=OFF
    -DFEATURE_ipc_posix=OFF

    # No inotify, therefore no filesystem watching.
    -DFEATURE_inotify=OFF
    -DFEATURE_fsnotify=OFF
    -DFEATURE_filesystemwatcher=OFF

    # Host-system probes and logging backends with no meaning in a wasm
    # sandbox (and whose config tests would consult the BUILD machine).
    -DFEATURE_glib=OFF
    -DFEATURE_icu=OFF
    -DFEATURE_journald=OFF
    -DFEATURE_syslog=OFF
    -DFEATURE_slog2=OFF
    -DFEATURE_lttng=OFF
    -DFEATURE_etw=OFF
    -DFEATURE_ctf=OFF
    -DFEATURE_backtrace=OFF
    -DFEATURE_cxx23_stacktrace=OFF
    -DFEATURE_getauxval=OFF
    -DFEATURE_renameat2=OFF
    -DFEATURE_linkat=OFF
    -DFEATURE_x86intrin=OFF

    # timezone is `CONDITION NOT WASM AND NOT VXWORKS`, i.e. ON for us -- and
    # qtimezoneprivate_tz.cpp scans /usr/share/zoneinfo and /etc/localtime,
    # paths a wk node's VFS would have to provide. Off until something needs
    # QTimeZone; the fix then is to ship a zoneinfo tree in the node image.
    -DFEATURE_timezone=OFF

    # NETWORK. On, because wk's fabric gives a node real BSD sockets and the
    # QPA plugin's dispatcher now polls fds (see qpa/qwkeventdispatcher.cpp).
    # QtNetwork is the one module where "a genuine WASI platform" costs the
    # most: Qt's own Emscripten build writes half these CONDITIONs as
    # `NOT WASM`, so for us they all autodetect back ON against a libc that
    # cannot honour them. Each OFF below names the thing that is missing.
    -DFEATURE_network=ON
    # Interface enumeration. CONDITION is bare `NOT WASM`, so it stays ON for
    # us -- and qnetworkinterface_unix.cpp then needs <net/if.h> (absent from
    # the wasi sysroot entirely) plus getifaddrs/if_nametoindex/if_indextoname
    # (headers present, symbols in NO library -- the eventfd trap again, see
    # patches/README.md). Off makes the file compile to nothing via
    # QT_NO_NETWORKINTERFACE. QHostAddress and QTcpSocket do not need it; only
    # multicast, link-local scope ids and QNetworkInterface itself do.
    -DFEATURE_networkinterface=OFF
    -DFEATURE_getifaddrs=OFF
    -DFEATURE_ipv6ifname=OFF
    -DFEATURE_linux_netlink=OFF
    # QDnsLookup, over ../resolv-compat. wasi-libc ships <arpa/nameser.h> --
    # every DNS constant and the BIND HEADER struct -- but no resolver, so
    # there is no <resolv.h> and upstream falls back to qdnslookup_dummy.cpp,
    # which errors on every lookup. ../resolv-compat supplies res_ninit,
    # res_nmkquery, res_nsend and dn_expand over ordinary sockets, which is all
    # Qt borrows: qdnslookup_unix.cpp parses every record type itself. Its
    # sysroot is on CMAKE_FIND_ROOT_PATH above, so Qt's own FindWrapResolv
    # probe (find_library(resolv) + a compile test) finds it.
    #
    # Passed ON explicitly, but do NOT trust it to fail loudly: Qt does not
    # error when a forced feature's CONDITION is false, it prints
    #   Resetting 'FEATURE_libresolv' from 'ON' to 'OFF' because it doesn't
    #   meet its condition 'WrapResolv_FOUND'
    # in the middle of thousands of configure lines and carries on, silently
    # linking qdnslookup_dummy.cpp — which builds, links, and then errors on
    # every lookup at runtime. If QDnsLookup ever starts returning
    # ResolverError, grep the configure log for that line first.
    #
    # This is separate from QHostInfo, which needs none of it -- plain
    # getaddrinfo() already reaches wk's fabric name service, and that is the
    # only thing that resolves sibling NODE names (plugins/fetch/fetch.c is the
    # reference). QDnsLookup is for the record types getaddrinfo cannot express:
    # MX, SRV, TXT, NS, PTR, CNAME, SOA.
    -DFEATURE_libresolv=ON
    # res_setservers() is glibc-specific and the shim does not provide it; Qt
    # has a documented fallback that writes _res.nsaddr_list directly, which it
    # does provide.
    -DFEATURE_res_setservers=OFF
    # No longer CONDITION QT_FEATURE_thread -- see
    # patches/qtbase-0010-dnslookup-without-threads.patch.
    -DFEATURE_dnslookup=ON
    # AF_UNIX: <sys/un.h> exists in the sysroot but wasi:sockets has no unix
    # domain at all, so QLocalSocket would compile and fail at runtime.
    -DFEATURE_localserver=OFF
    # <netinet/sctp.h> absent.
    -DFEATURE_sctp=OFF
    # A wk node reaches the outside world through its Network's gateway, not
    # through a proxy, and system_proxies would consult the BUILD machine.
    -DFEATURE_networkproxy=OFF
    -DFEATURE_socks5=OFF
    -DFEATURE_system_proxies=OFF
    -DFEATURE_libproxy=OFF
    -DFEATURE_networklistmanager=OFF
    # topleveldomain is AUTODETECT NOT WASM, i.e. ON for us, and compiles a
    # ~200KB binary dump of the Public Suffix List into every node just so the
    # cookie jar can reject supercookies. Off until a node needs cookies.
    -DFEATURE_topleveldomain=OFF
    -DFEATURE_publicsuffix_qt=OFF
    -DFEATURE_publicsuffix_system=OFF
    # No brotli/zstd/gssapi in the sysroot; they would fail their find_package
    # anyway, but pinning them keeps the configure output stable.
    -DFEATURE_brotli=OFF
    -DFEATURE_gssapi=OFF
    # QNetworkDiskCache wants a writable cache dir in QStandardPaths; a node's
    # vfs may not have one. The in-memory cache still works.
    -DFEATURE_networkdiskcache=OFF
    # QUdpSocket. The three datagram functions in qnativesocketengine_unix.cpp
    # sit OUTSIDE QT_CONFIG(udpsocket), so turning this off does NOT remove
    # their calls to recvmsg/sendmsg -- which wasi-libc declares and never
    # defines. patches/qtbase-0008 supplies them over recvfrom/sendto, so the
    # feature costs nothing extra and QUdpSocket links. It is NOT proven over
    # the fabric; see PORTING.md.
    -DFEATURE_udpsocket=ON
    # HTTP. CONDITION is QT_FEATURE_thread upstream, because Qt 6.8's backend
    # runs QHttpThreadDelegate on a QThread it creates. patches/qtbase-0009
    # relaxes that and makes the delegate live on the calling thread, which is
    # sound here precisely BECAUSE there are no threads: with QT_CONFIG(thread)
    # off, qobject.cpp:4094 compiles BlockingQueuedConnection out and every
    # such emit becomes a direct call. See the patch header for the argument.
    -DFEATURE_http=ON
    -DFEATURE_sql=OFF
    -DFEATURE_testlib=OFF
    -DFEATURE_printsupport=OFF
    -DFEATURE_dbus=OFF
    # TLS. There is no TLS backend for this platform: securetransport is
    # CONDITION APPLE (the target is not), schannel is WIN32, and no OpenSSL is
    # cross-built for wasm32-wasip2 here. So QT_FEATURE_ssl is 0 and https://
    # simply does not exist for QNetworkAccessManager. Pinned rather than left
    # derived so that the day someone cross-builds OpenSSL, this line is the
    # one they have to delete on purpose.
    -DFEATURE_ssl=OFF
    -DFEATURE_dtls=OFF
    -DFEATURE_ocsp=OFF
    -DFEATURE_openssl=OFF
    -DFEATURE_openssl_linked=OFF
    -DFEATURE_openssl_hash=OFF
    -DFEATURE_zstd=OFF

    # No GPU of any kind. wk gives a node an RGBA8 framebuffer; every path here
    # is raster.
    -DINPUT_opengl=no
    -DFEATURE_opengl=OFF
    -DFEATURE_opengl_desktop=OFF
    -DFEATURE_opengles2=OFF
    -DFEATURE_opengles3=OFF
    -DFEATURE_opengles31=OFF
    -DFEATURE_opengles32=OFF
    -DFEATURE_egl=OFF
    -DFEATURE_openvg=OFF
    -DFEATURE_vulkan=OFF
    -DFEATURE_vkgen=OFF
    -DFEATURE_vkkhrdisplay=OFF
    -DFEATURE_metal=OFF
    -DFEATURE_graphicsframecapture=OFF

    # No host windowing system, no host input stack.
    -DFEATURE_xcb=OFF
    -DFEATURE_xlib=OFF
    -DFEATURE_xkbcommon=OFF
    -DFEATURE_xkbcommon_x11=OFF
    -DFEATURE_wayland=OFF
    -DFEATURE_directfb=OFF
    -DFEATURE_linuxfb=OFF
    -DFEATURE_vnc=OFF
    -DFEATURE_kms=OFF
    -DFEATURE_gbm=OFF
    -DFEATURE_eglfs=OFF
    -DFEATURE_integrityfb=OFF
    -DFEATURE_evdev=OFF
    -DFEATURE_libinput=OFF
    -DFEATURE_libudev=OFF
    -DFEATURE_mtdev=OFF
    -DFEATURE_tslib=OFF
    -DFEATURE_tuiotouch=OFF
    -DFEATURE_sessionmanager=OFF
    # accessibility's bridge is AT-SPI over D-Bus, which we do not have.
    -DFEATURE_accessibility=OFF
    -DFEATURE_systemtrayicon=OFF

    # Text and images: everything bundled, nothing from the host.
    # qtbase/src/3rdparty carries freetype, harfbuzz, libjpeg, libpng, zlib,
    # pcre2, md4c and double-conversion, so no external sysroot is needed --
    # and every system_* must be OFF or a stray /opt/homebrew header gets in.
    -DFEATURE_gui=ON
    -DFEATURE_widgets=ON
    -DFEATURE_freetype=ON
    -DFEATURE_system_freetype=OFF
    -DFEATURE_harfbuzz=ON
    -DFEATURE_system_harfbuzz=OFF
    -DFEATURE_png=ON
    -DFEATURE_system_png=OFF
    -DFEATURE_jpeg=ON
    -DFEATURE_system_jpeg=OFF
    -DFEATURE_gif=ON
    -DFEATURE_fontconfig=OFF
    -DFEATURE_system_zlib=OFF
    -DFEATURE_system_pcre2=OFF
    -DFEATURE_system_doubleconversion=OFF
    -DFEATURE_system_libb2=OFF
    -DFEATURE_raster_64bit=ON
    -DFEATURE_raster_fp=ON

    # Build hygiene. pkg_config would answer from the host's .pc files; ltcg
    # (LTO) re-runs codegen at link time where the -mllvm EH flags are not in
    # effect and produces a component wasmtime rejects; rpath and
    # separate_debug_info are meaningless for a static wasm component.
    -DFEATURE_pkg_config=OFF
    -DFEATURE_ltcg=OFF
    -DFEATURE_precompile_header=OFF
    -DFEATURE_reduce_relocations=OFF
    -DFEATURE_rpath=OFF
    -DFEATURE_separate_debug_info=OFF
    -DFEATURE_relocatable=OFF
)

# --- configure --------------------------------------------------------------
LOG="$LOGDIR/target-qtbase.log"
if [ ! -f "$BUILD/CMakeCache.txt" ] || [ -n "${WK_QT_RECONFIGURE:-}" ]; then
    echo "=== configuring qtbase $QT_VER for wasm32-wasip2 (log: $LOG)"
    env PATH="$BUILD_PATH" cmake "${CFG[@]}" 2>&1 | tee "$LOG"
else
    echo "=== qtbase already configured in $BUILD (WK_QT_RECONFIGURE=1 to redo)"
fi

# --- a look at what configure actually decided -------------------------------
# Cheap, and it catches the silent-failure modes early. thread must be OFF.
#
# poll_ppoll must be ON: wasi-sdk 34-rc.2 DOES define ppoll (llvm-nm over
# libc.a shows `T ppoll`) and poll.h declares it, so the config test passes
# honestly. That is load-bearing rather than incidental -- on wasip2 ppoll() IS
# a single wasi:io/poll.poll over the descriptors' pollables plus a
# monotonic-clock deadline, which is what lets QWkEventDispatcher put the wk
# frame, Qt's timers and every QSocketNotifier fd into ONE blocking call.
#
# The four files are not interchangeable, and looking in only one of them is
# why this check used to print two of the five things it claimed to check:
# public GLOBAL features land in global/qconfig.h (`thread`), private global
# ones in global/qconfig_p.h (`dlopen`), public per-module ones in
# qtcore-config.h (`process`, `library`, `timezone`) and private per-module
# ones in qtcore-config_p.h (`poll_ppoll`).
for f in global/qconfig.h global/qconfig_p.h qtcore-config.h qtcore-config_p.h; do
    [ -f "$BUILD/src/corelib/$f" ] || continue
    grep -HE "QT_FEATURE_(thread|poll_ppoll|dlopen|process|timezone|library) " \
        "$BUILD/src/corelib/$f" || true
done | sed 's|^.*/||;s|^|    |' | sort -u | \
    { echo "--- configure sanity (thread/dlopen/process/timezone/library -1, poll_ppoll 1)"; cat; }
if [ -f "$BUILD/src/network/qtnetwork-config.h" ]; then
    echo "--- network sanity (http 1, ssl 0, networkinterface 0)"
    grep -E "QT_FEATURE_(http|ssl|udpsocket|networkinterface|dnslookup|localserver) " \
        "$BUILD/src/network/qtnetwork-config.h" || true
fi

# --- build + install ---------------------------------------------------------
# Staged on purpose: Core is where every wasi-libc gap lives, so failing there
# is ten minutes of feedback instead of sixty. WK_QT_STAGES overrides
# ("Core" to stop after QtCore; "all" to skip straight to everything).
STAGES="${WK_QT_STAGES:-Core Network Gui Widgets all}"
for stage in $STAGES; do
    if [ "$stage" = all ]; then
        echo "=== ninja (everything)"
        env PATH="$BUILD_PATH" cmake --build "$BUILD" --parallel "$JOBS" 2>&1 | tee -a "$LOG"
    else
        echo "=== ninja $stage"
        env PATH="$BUILD_PATH" cmake --build "$BUILD" --parallel "$JOBS" --target "$stage" 2>&1 | tee -a "$LOG"
    fi
done

env PATH="$BUILD_PATH" cmake --install "$BUILD" 2>&1 | tee -a "$LOG"

echo
echo "qtbase $QT_VER (wasm32-wasip2) installed in $SYSROOT"
ls "$SYSROOT/lib"/libQt6*.a 2>/dev/null || echo "  (no libQt6*.a -- check $LOG)"
