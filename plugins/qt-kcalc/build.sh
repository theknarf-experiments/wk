#!/usr/bin/env bash
# KCalc — a REAL KDE application, KDE Frameworks 6 and all, as a wk node.
#
# Produces ./kcalc.wasm: a wasm32-wasip2 COMPONENT that imports
# wasi:surface/graphics-context/frame-buffer and paints through the wk QPA
# plugin, exactly like plugins/qt-torrentfileeditor103 does — except that the
# thing above Qt here is not one app's own code, it is FOURTEEN KDE Frameworks.
#
# WHY THIS PORT EXISTS. torrent-file-editor proved "a Qt app runs in wk".
# KCalc is the strictly harder question: does the *KDE platform* run in wk?
# Frameworks are where the POSIX assumptions live — QProcess, dlopen'd plugins,
# gettext, DBus, inotify, /proc — so a KF6 stack that RUNS is a much stronger
# statement about wasip2 than any single app. `harness/` is where "runs" is
# checked: it drives the component on wk's real PluginHost and asserts that
# KCalc evaluated 8÷2 and 1÷8 through KNumber, i.e. through GMP and MPFR.
#
# WHAT THIS SCRIPT IS, in one line: six third-party cross-builds, FOURTEEN KDE
# framework cross-builds, one native host tool, and a link.
#
#   zlib     -> ./sysroot   (KArchive's only mandatory compressor)
#   libintl  -> ./sysroot   (KI18n hard-requires one; wasi-sdk has none)
#   qtsvg    -> ./sysroot   (KIconThemes hard-requires Qt6::Svg)
#   gmp      -> ./sysroot   \
#   mpfr     -> ./sysroot    > KCalc's knumber makes all three TYPE REQUIRED
#   mpc      -> ./sysroot   /
#   KF6 x14  -> ./sysroot   (the dependency order is in KF_ORDER below)
#   kconfig_compiler -> ./host-tooling   (NATIVE; see the kconfighost stage)
#   kcalc    -> ./kcalc.wasm
#
# The fifteenth framework, KCrash, is NOT built. See the long note above
# KF_ORDER and PORTING.md — it is the one thing in KCalc's graph with no
# meaningful wasm implementation, and this port does not pretend otherwise.
#
# WHY A PORT-LOCAL SYSROOT. plugins/qt/sysroot holds qtbase + the wk QPA and is
# shared by every Qt port. Everything this port adds installs into ./sysroot
# HERE, so several Qt ports can grow different module sets from one qtbase
# without writing into each other's trees. Both prefixes go on
# CMAKE_PREFIX_PATH and CMAKE_FIND_ROOT_PATH. Same pattern as
# plugins/qt-torrentfileeditor103.
#
# ---------------------------------------------------------------------------
# THE FOUR WALLS, and why none of them is DBus
# ---------------------------------------------------------------------------
# The obvious guess is that KDE needs DBus and wasm has no DBus. That guess is
# WRONG, and the reason is a happy accident of CMake: every framework in
# KCalc's graph gates DBus behind
#     set(USE_DBUS_DEFAULT OFF)
#     if(UNIX AND NOT APPLE AND NOT ANDROID AND NOT HAIKU) ... ON ... endif()
#     option(USE_DBUS "" ${USE_DBUS_DEFAULT})
# and CMake does NOT set UNIX for CMAKE_SYSTEM_NAME=WASI (verified by probe:
# `UNIX=[] WASI=[1]`). Qt gets UNIX only because plugins/qt patches it in for
# its own build; that patch does not reach us. So USE_DBUS is OFF by default
# here with no flag at all, and KCalc's graph never touches KIO, KService,
# KDBusAddons, KGlobalAccel, KAuth or Solid — the frameworks that genuinely
# cannot lose DBus. We pass -DUSE_DBUS=OFF anyway, to say it on purpose.
#
# The real walls are:
#   1. QT VERSION. KF6 master needs Qt 6.9.0; plugins/qt is 6.8.4. KF 6.24.0 is
#      the NEWEST tag that still requires only Qt 6.8.0, which is why every
#      clone below is pinned to v6.24.0 and not to master.
#   2. MISSING QT MODULES. KXmlGui hard-requires Network and PrintSupport.
#      Network is now ON in plugins/qt (see plugins/qt/build-qtbase.sh); print
#      support is not, and PrintSupport lives INSIDE qtbase, so the port-local
#      overlay rule cannot save us — it has to be patched out of KXmlGui.
#      KIconThemes needs Svg, which IS a separate repo and IS layerable.
#   3. HARD `REQUIRED`s THAT IGNORE THEIR OWN SWITCHES. KCrash unconditionally
#      find_package(Qt6Test REQUIRED) at top-level scope (it configures after
#      that patch and still does not compile — see KF_ORDER); KNotifications
#      unconditionally find_package(Canberra REQUIRED) and Qt6 DBus on any
#      non-Apple/Android/Win/Haiku platform. Both are CMake bugs, not code
#      dependencies — every source they guard is already gated correctly.
#   4. THE NO-THREADS / NO-PROCESS / NO-DLOPEN TAX. Concentrated in
#      KCoreAddons: QProcess (FEATURE_process=OFF), QLibrary/QPluginLoader
#      (FEATURE_library=OFF), QTimeZone (FEATURE_timezone=OFF), socketpair,
#      and struct rlimit. See patches/README.md — each one is a named,
#      bounded fix, not a stub of KDE behaviour.
#
# Plus one that is upstream telling you you are off the map: ECM refuses the
# WASI platform outright (KDEMetaInfoPlatformCheck.cmake), and ships
# KF_IGNORE_PLATFORM_CHECK as the documented escape hatch. We use it.
#
# TRANSLATIONS. Every framework's top-level CMakeLists calls
# ecm_install_po_files_as_qm(poqm), which hard-requires Qt6 LinguistTools — a
# HOST tool, and plugins/qt has no qttools build. So each tree's poqm/ is
# deleted after fetch and before configure. Combined with the passthrough
# libintl this makes the whole stack English-only BY CONSTRUCTION; it also
# means no .mo catalogs need staging into the node's vfs, because
# KCatalog::translate returns the msgid unchanged when it finds none.
#
# EVERYTHING INHERITED FROM plugins/qt, and the traps that come with it — read
# plugins/qt/wasip2.cmake's header, it is the primary document:
#   * the exnref EH flag set. wasmtime runs with the exception proposal ON and
#     REJECTS wasi-sdk's default legacy encoding, at instantiate time, for the
#     WHOLE component — one stray object poisons the binary;
#   * the wasm-opt trap: clang runs wasm-opt as an optional post-link pass and
#     the wasm-opt on PATH cannot parse exnref. Every cmake/ninja call here runs
#     under a PATH that omits it, INCLUDING the build step, because CMake bakes
#     absolute tool paths into build.ninja;
#   * no threads, no dlopen: the wk QPA, the image-format plugins and the qsvg
#     plugins are all STATIC and named with Q_IMPORT_PLUGIN.
#
# Knobs: WK_KCALC_STAGES="ecm zlib libintl qtsvg gmp mpfr mpc kf kconfighost app"
#        JOBS=N   LOGDIR=...   WK_KCALC_RECONFIGURE=1   QT_HOST_PATH=...
#        WK_KCALC_KF="kcoreaddons kconfig ..."   (subset of KF_ORDER)
#
# Long: budget 40-70 minutes cold. Run it detached and tail ./logs.
set -euo pipefail
cd "$(dirname "$0")"

# --- toolchain guard (same shape as plugins/qt-torrentfileeditor103) ---------
MISE_SDK="$HOME/.local/share/mise/installs/github-web-assembly-wasi-sdk/wasi-sdk-34-rc.2"
WASI_SDK="${WASI_SDK:-$([ -d "$MISE_SDK" ] && echo "$MISE_SDK" || echo "$HOME/wasi-sdk")}"
EXPECT="wasi-sdk-34-rc.2"
case "$WASI_SDK" in
    *"$EXPECT"*) ;;
    *) echo "qt-kcalc: expected $EXPECT (set WASI_SDK), got: $WASI_SDK" >&2; exit 1 ;;
esac
export WASI_SDK

QT_VER=6.8.4
QT_SERIES=6.8
# KF 6.24.0 is the newest KDE Frameworks tag whose REQUIRED_QT_VERSION is still
# 6.8.0. 6.25.0 and later demand Qt 6.9.0, which plugins/qt is not. Do not bump
# this without bumping qtbase first.
KF_VER=v6.24.0
# KDE Gear release that carries KCalc. 26.04.3's own floor is Qt 6.5.0 /
# KF 6.0.0, so the KF 6.24.0 pin costs nothing here.
KCALC_VER=26.04.3
ZLIB_VER=1.3.1
GMP_VER=6.3.0
MPFR_VER=4.2.1
MPC_VER=1.3.1

HERE="$PWD"
QTPLUGIN="$PWD/../qt"                 # the shared Qt port: qtbase + the wk QPA
QTBASE_SYSROOT="$QTPLUGIN/sysroot"
HOST_QT="${QT_HOST_PATH:-$QTPLUGIN/host}"
GFXCOMPAT="$PWD/../gfx-compat"
CLIPCOMPAT="$PWD/../clipboard-compat"

SRCDIR="$PWD/src"
TARBALLS="$PWD/tarballs"
PATCHDIR="$PWD/patches"
SYSROOT="$PWD/sysroot"                # OUR wasm install prefix (everything)
HOSTPREFIX="$PWD/host"                # OUR host prefix (extra-cmake-modules)
HOSTTOOLING="$PWD/host-tooling"       # native kconfig_compiler; see the stage
BUILD="$PWD/build"
GEN="$PWD/gen"                        # our own wit-bindgen output
FONTDIR="$PWD/fonts"
LOGDIR="${LOGDIR:-$PWD/logs}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || nproc)}"
BUILD_PATH="$WASI_SDK/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$SRCDIR" "$TARBALLS" "$LOGDIR" "$BUILD" "$GEN" "$FONTDIR" "$SYSROOT" "$HOSTPREFIX" "$HOSTTOOLING"

# --- preflight --------------------------------------------------------------
if [ ! -f "$QTBASE_SYSROOT/lib/libQt6Widgets.a" ]; then
    echo "qt-kcalc: no cross Qt in $QTBASE_SYSROOT" >&2
    echo "  run plugins/qt/build-qtbase.sh first (it is the long one)." >&2
    exit 1
fi
if [ ! -f "$QTBASE_SYSROOT/lib/libqwk.a" ]; then
    echo "qt-kcalc: no wk QPA plugin at $QTBASE_SYSROOT/lib/libqwk.a" >&2
    echo "  run plugins/qt/build-qpa.sh first." >&2
    exit 1
fi
if [ ! -x "$HOST_QT/libexec/moc" ] && [ ! -x "$HOST_QT/bin/moc" ]; then
    echo "qt-kcalc: no host Qt at $HOST_QT -- run plugins/qt/build-host.sh" >&2
    exit 1
fi

# --- upstream, fetched not vendored -----------------------------------------
fetch_tar() {  # fetch_tar <url> <expected-dir-under-src>
    local url="$1" dir="$SRCDIR/$2" f
    f="$TARBALLS/$(basename "$url")"
    discard_unpatched_tree "$dir"
    [ -d "$dir" ] && return 0
    if [ ! -f "$f" ]; then
        echo "fetching $(basename "$url")..."
        curl -fsSL --retry 3 -o "$f.part" "$url"
        mv "$f.part" "$f"
    fi
    echo "extracting $(basename "$url")..."
    case "$f" in
        *.tar.xz)  tar xJf "$f" -C "$SRCDIR" ;;
        *.tar.gz)  tar xzf "$f" -C "$SRCDIR" ;;
        *) echo "qt-kcalc: unknown archive $f" >&2; exit 1 ;;
    esac
}

fetch_kf() {   # fetch_kf <repo>; a shallow clone pinned to $KF_VER
    local r="$1"
    [ -d "$SRCDIR/$r/.git" ] && return 0
    rm -rf "${SRCDIR:?}/$r"
    echo "cloning $r $KF_VER..."
    git clone --quiet --depth 1 -b "$KF_VER" \
        "https://invent.kde.org/frameworks/$r.git" "$SRCDIR/$r"
}

# --- patches ----------------------------------------------------------------
# patches/<prefix>-NNNN-*.patch, applied -p1 at that tree's root. See
# patches/README.md for the ledger and the WHY of each one.
#
# Two idempotency strategies, because there are two kinds of tree here:
#
#   * KF trees are GIT CLONES, so the honest reset is `git reset --hard` +
#     `git clean -fd` and then re-apply from scratch. That is cheap (no
#     re-clone), it cannot leave a half-applied tree, and — unlike the
#     reverse-check idiom in plugins/qt/build-qtbase.sh — it keeps working when
#     two patches touch nearby lines.
#   * TARBALL trees have no git, so they get a .wk-patched stamp, and an
#     unstamped tree is re-extracted (it is either pristine or wreckage).
apply_patches_git() {
    local tree="$1" prefix="$2" p
    git -C "$tree" reset --hard --quiet
    git -C "$tree" clean -fdq
    for p in "$PATCHDIR/$prefix"-*.patch; do
        [ -e "$p" ] || continue
        echo "  patch: $(basename "$p")"
        git -C "$tree" apply "$p"
    done
    # Deleted AFTER the patches so a patch can still refer to it if it ever
    # needs to. ecm_install_po_files_as_qm(poqm) hard-requires Qt6
    # LinguistTools, a host tool plugins/qt has no qttools build to provide;
    # translations are out of scope for this port anyway (see the
    # passthrough-libintl note in the header).
    rm -rf "$tree/poqm"
}

# `patch -p1`, NOT `git apply`, and this one cost an hour — write it down.
#
# A tarball tree has no .git of its own, but it lives at
# plugins/qt-kcalc/src/<tree>, which is INSIDE the wk repository. `git apply`
# run there therefore discovers the wk repo, computes a prefix of
# "plugins/qt-kcalc/src/<tree>/", and then compares that prefix against the
# paths in the patch. A patch produced by `git diff` carries
# `diff --git a/CMakeLists.txt b/CMakeLists.txt`, git takes those paths
# literally, they do not start with the prefix — and git prints
#     Skipped patch 'CMakeLists.txt'.
# and EXITS 0. Nothing fails. The build then configures an unpatched tree and
# the error surfaces somewhere else entirely ("Could NOT find KF6Crash").
#
# plugins/qt-torrentfileeditor103 does use `git apply` here and gets away with
# it only because its patches are plain `diff -u` output with no `diff --git`
# line — in that case git falls back to prepending the prefix and it works.
# That is an accident, not a design. `patch -p1` has no notion of an enclosing
# repository, handles both patch formats, and fails loudly.
apply_patches() {
    local tree="$1" prefix="$2" p
    [ -f "$tree/.wk-patched" ] && { echo "  patches: already applied to $(basename "$tree")"; return 0; }
    for p in "$PATCHDIR/$prefix"-*.patch; do
        [ -e "$p" ] || continue
        echo "  patch: $(basename "$p")"
        patch -p1 -d "$tree" --forward < "$p"
    done
    touch "$tree/.wk-patched"
}

discard_unpatched_tree() {
    local tree="$1"
    if [ -d "$tree" ] && [ ! -f "$tree/.wk-patched" ]; then
        echo "  re-extracting $(basename "$tree") (no .wk-patched stamp)"
        rm -rf "$tree"
    fi
}

# --- a CMake cross-build into our sysroot -----------------------------------
cross_cmake() {  # cross_cmake <name> <srcdir> [extra -D...]
    local name="$1" src="$2"; shift 2
    local bld="$BUILD/$name" log="$LOGDIR/$name.log"
    # build.ninja, not CMakeCache.txt: a configure that FAILED still leaves a
    # cache behind, and keying off it makes the next run skip configure and die
    # with "ninja: error: loading 'build.ninja'".
    if [ ! -f "$bld/build.ninja" ] || [ -n "${WK_KCALC_RECONFIGURE:-}" ]; then
        echo "=== configuring $name for wasm32-wasip2 (log: $log)"
        env PATH="$BUILD_PATH" cmake -G Ninja -S "$src" -B "$bld" \
            -DCMAKE_TOOLCHAIN_FILE="$QTPLUGIN/wasip2.cmake" \
            -DWASI_SDK_PREFIX="$WASI_SDK" \
            -DCMAKE_FIND_ROOT_PATH="$QTBASE_SYSROOT;$SYSROOT;$HOSTPREFIX" \
            -DCMAKE_PREFIX_PATH="$QTBASE_SYSROOT;$SYSROOT;$HOSTPREFIX" \
            -DQT_HOST_PATH="$HOST_QT" \
            -DCMAKE_INSTALL_PREFIX="$SYSROOT" \
            -DCMAKE_BUILD_TYPE=Release \
            -DBUILD_SHARED_LIBS=OFF \
            "$@" 2>&1 | tee "$log"
    else
        echo "=== $name already configured in $bld (WK_KCALC_RECONFIGURE=1 to redo)"
    fi
    echo "=== ninja $name"
    env PATH="$BUILD_PATH" cmake --build "$bld" --parallel "$JOBS" 2>&1 | tee -a "$log"
    env PATH="$BUILD_PATH" cmake --install "$bld" 2>&1 | tee -a "$log"
}

# --- a KDE framework cross-build --------------------------------------------
# The preamble below is not decoration. Every flag is here for a failure that
# actually happened; do not trim it.
#
#   HOSTPREFIX on CMAKE_FIND_ROOT_PATH — wasip2.cmake sets
#     CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY, so find_package(ECM) cannot see a
#     prefix that is only on CMAKE_PREFIX_PATH.
#   KF_IGNORE_PLATFORM_CHECK — ECM's KDEMetaInfoPlatformCheck.cmake FATAL_ERRORs
#     on 'WASI' and names this variable in the message as the way through.
#   BUILD_TESTING=OFF — the autotests need Qt6Test, which plugins/qt does not
#     build (FEATURE_testlib=OFF).
#   ENABLE_PCH=OFF — precompiled headers plus the -mllvm EH flags are a fight
#     not worth having.
#   USE_DBUS=OFF — already the default here (see the header), said on purpose.
#   BUILD_PYTHON_BINDINGS=OFF — needs shiboken and a host Python Qt.
build_kf() {  # build_kf <repo> [extra -D...]
    local repo="$1"; shift
    fetch_kf "$repo"
    apply_patches_git "$SRCDIR/$repo" "$repo"
    cross_cmake "$repo" "$SRCDIR/$repo" \
        -DBUILD_TESTING=OFF \
        -DBUILD_PYTHON_BINDINGS=OFF \
        -DENABLE_PCH=OFF \
        -DUSE_DBUS=OFF \
        -DKF_IGNORE_PLATFORM_CHECK=TRUE \
        "$@"
}

# Dependency order: each entry depends only on entries before it.
#
# KCRASH IS NOT IN THIS LIST, and that is the single loudest fact about this
# port. KCrash's implementation is sigaction()/sigemptyset()/SA_RESTART signal
# handlers that, on a crash, fork() a child, setgroups()/setgid()/setuid() it,
# exec() drkonqi, waitpid() for it and alarm() a watchdog. Fifteen distinct
# undefined identifiers in kcrash.cpp alone (fork, waitpid, kill, alarm,
# setuid, setgid, getuid, getgid, setgroups, sigaction, sigemptyset, sigaddset,
# SA_RESTART, SIG_UNBLOCK, RLIMIT_NOFILE) plus <QUnhandledException> in
# exception.cpp. wasip2 has no asynchronous signals and no fork/exec at all.
#
# patches/kcrash-0001 DOES exist and fixes a real upstream bug (an
# unconditional find_package(Qt6Test REQUIRED) at top-level scope), so KCrash
# now CONFIGURES -- and then fails to compile. It is kept so that anyone
# re-attempting this starts past the configure wall rather than at it:
#
#     WK_KCALC_STAGES=kf WK_KCALC_KF=kcrash ./build.sh
#
# reproduces the compile failure in about a minute. Building a KCrash that
# compiled would mean gutting kcrash.cpp until KCrash::initialize() did
# nothing, which is a stub wearing a framework's name -- worse than an honest
# absence. So KCalc drops KF6::Crash entirely (patches/kcalc-0001) and runs
# with no crash handler: when a wk node traps, wasmtime reports it to the host
# and the node dies. See PORTING.md.
KF_ORDER="kcoreaddons kcodecs kwidgetsaddons kguiaddons ki18n kconfig kcolorscheme kconfigwidgets karchive kiconthemes kitemviews kbookmarks kxmlgui knotifications"

# Per-framework flags. Everything here turns OFF something that would otherwise
# be REQUIRED and unbuildable, or that would drag QtQuick/QML into a Widgets
# app. The reason is on each line because six months from now it will not be
# obvious which of these are load-bearing.
kf_stage() {
    local repo="$1"
    case "$repo" in
    kcoreaddons)
        # KCOREADDONS_USE_QML pulls Qt6Qml, which plugins/qt does not build.
        # ENABLE_INOTIFY: KDirWatch's inotify backend. wasi-libc has no
        # sys/inotify.h; KDirWatch falls back to its stat-polling backend,
        # which is the right answer on a vfs with no change notification.
        build_kf kcoreaddons -DKCOREADDONS_USE_QML=OFF -DENABLE_INOTIFY=OFF ;;
    kguiaddons)
        # The Wayland/X11 backends of KModifierKeyInfo and KKeySequenceRecorder.
        # Neither exists here; both default to a probe that must not find the
        # host's.
        build_kf kguiaddons -DWITH_WAYLAND=OFF -DWITH_X11=OFF ;;
    ki18n)
        # BUILD_WITH_QML pulls Qt6Qml. Points at the passthrough libintl in
        # ./sysroot via the shared CMAKE_FIND_ROOT_PATH.
        build_kf ki18n -DBUILD_WITH_QML=OFF ;;
    kconfig)
        # KCONFIG_USE_QML defaults ON and pulls Qml+Quick. KCONFIG_USE_GUI
        # stays ON — KConfigGui is what KConfigWidgets is built on.
        build_kf kconfig -DKCONFIG_USE_QML=OFF ;;
    karchive)
        # All four of these default ON and each promotes its package to
        # REQUIRED. Only zlib is cross-built here, and zlib is all KIconThemes
        # actually needs (icon theme caches are uncompressed; the compressors
        # matter for KIO's tar/zip handling, which is not in this graph).
        build_kf karchive -DWITH_BZIP2=OFF -DWITH_LIBLZMA=OFF \
                          -DWITH_OPENSSL=OFF -DWITH_LIBZSTD=OFF ;;
    kiconthemes)
        # KICONTHEMES_USE_QTQUICK defaults ON and pulls Qml+Quick.
        # USE_BreezeIcons=OFF avoids compiling KF6BreezeIcons — a qrc holding
        # several thousand SVGs — into every binary. Icons then come from the
        # node's vfs via XDG_DATA_DIRS, and a missing theme renders blank
        # rather than crashing.
        build_kf kiconthemes -DKICONTHEMES_USE_QTQUICK=OFF -DUSE_BreezeIcons=OFF ;;
    *)
        build_kf "$repo" ;;
    esac
}

# --- an autotools cross-build into our sysroot ------------------------------
# GMP, MPFR and MPC only. Nothing else in this port is autotools and nothing
# should become so.
autotools_build() {
    local name="$1" src bld log
    case "$name" in
        gmp)  src="$SRCDIR/gmp-$GMP_VER";   fetch_tar "https://gmplib.org/download/gmp/gmp-$GMP_VER.tar.xz" "gmp-$GMP_VER" ;;
        mpfr) src="$SRCDIR/mpfr-$MPFR_VER"; fetch_tar "https://ftp.gnu.org/gnu/mpfr/mpfr-$MPFR_VER.tar.xz" "mpfr-$MPFR_VER" ;;
        mpc)  src="$SRCDIR/mpc-$MPC_VER";   fetch_tar "https://ftp.gnu.org/gnu/mpc/mpc-$MPC_VER.tar.gz" "mpc-$MPC_VER" ;;
    esac
    apply_patches "$src" "$name"
    bld="$BUILD/$name"; log="$LOGDIR/$name.log"
    mkdir -p "$bld"

    # The EH flags have to be on these too. They are C, and C does not throw —
    # but -fwasm-exceptions is ALSO what selects wasi-sdk 34's eh/ variant of
    # the runtime libraries and puts lib/wasm32-wasip2/eh on the search path.
    # An object built without it links against the non-eh runtime, and the
    # mismatch surfaces at instantiate time as wasmtime rejecting the whole
    # component. See plugins/qt/wasip2.cmake's header.
    local CF="-Os -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID -fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false"
    local LDF="-lunwind -lsetjmp -lwasi-emulated-signal -lwasi-emulated-mman -lwasi-emulated-process-clocks -lwasi-emulated-getpid"

    # VAR=VALUE only. `env` must NOT be handed the --host/--prefix flags: it
    # parses a leading `--host=...` as one of ITS OWN options and dies with
    # "env: illegal option -- h" — while still exiting 0 through the pipe, so
    # the failure shows up much later as a missing library.
    local envv=(
        CC="$WASI_SDK/bin/clang" AR="$WASI_SDK/bin/llvm-ar"
        RANLIB="$WASI_SDK/bin/llvm-ranlib" NM="$WASI_SDK/bin/llvm-nm"
        CFLAGS="--target=wasm32-wasip2 $CF"
        LDFLAGS="--target=wasm32-wasip2 $LDF"
    )
    local common=( --host=wasm32-wasi --prefix="$SYSROOT" --enable-static --disable-shared )
    if [ ! -f "$bld/Makefile" ] || [ -n "${WK_KCALC_RECONFIGURE:-}" ]; then
        echo "=== configuring $name for wasm32-wasip2 (log: $log)"
        case "$name" in
        gmp)
            # --disable-assembly: GMP ships hand-written asm for every real CPU
            # and none for wasm; without this the generic path still emits
            # longlong.h inline asm that will not assemble.
            # NOT ABI=32, even though wasm32 is ILP32: for a CPU it does not
            # recognise GMP offers exactly one ABI and rejects anything else
            # with "configure: error: ABI=32 is not among the following valid
            # choices: standard". The `standard` ABI takes its limb size from
            # the C `long`, which is 32 bits on wasm32 -- the right answer,
            # arrived at by not interfering.
            ( cd "$bld" && env "${envv[@]}" "$src/configure" \
                "${common[@]}" --disable-assembly ) 2>&1 | tee "$log" ;;
        mpfr)
            # --disable-thread-safe: MPFR's thread-safety is TLS for its
            # exponent/rounding state, and wk qtbase is FEATURE_thread=OFF —
            # there is no second thread to protect it from.
            ( cd "$bld" && env "${envv[@]}" "$src/configure" \
                "${common[@]}" --with-gmp="$SYSROOT" --disable-thread-safe ) 2>&1 | tee "$log" ;;
        mpc)
            ( cd "$bld" && env "${envv[@]}" "$src/configure" \
                "${common[@]}" --with-gmp="$SYSROOT" --with-mpfr="$SYSROOT" ) 2>&1 | tee "$log" ;;
        esac
    else
        echo "=== $name already configured in $bld"
    fi
    echo "=== make $name"
    make -C "$bld" -j"$JOBS" 2>&1 | tee -a "$log"
    make -C "$bld" install 2>&1 | tee -a "$log"
}

# --- the app ----------------------------------------------------------------
app_stage() {
    # --- stage a font --------------------------------------------------------
    # Qt 6 ships none and a wk node has no host font directory: with no font
    # the app runs, QFontDatabase is empty, and every string renders as nothing
    # at all. Nothing is fetched from the network — a build must not depend on
    # a font download. Repo-local candidates first so a machine-independent
    # font is preferred.
    local FONT_CANDIDATES=(
        "$PWD/../doctools/tex/texlive-source/libs/gd/libgd-src/tests/freetype/DejaVuSans.ttf"
        "$QTPLUGIN/smoke/fonts/DejaVuSans.ttf"
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
        "/usr/share/fonts/dejavu/DejaVuSans.ttf"
        "/Library/Fonts/Arial.ttf"
        "/System/Library/Fonts/Supplemental/Arial.ttf"
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf"
    )
    local FONT="" f
    for f in "${FONT_CANDIDATES[@]}"; do
        if [ -f "$f" ]; then FONT="$f"; break; fi
    done
    if [ -z "$FONT" ]; then
        echo "qt-kcalc: no font found. Tried:" >&2
        printf '  %s\n' "${FONT_CANDIDATES[@]}" >&2
        exit 1
    fi
    cp -f "$FONT" "$FONTDIR/$(basename "$FONT")"
    echo "=== font: $FONT -> fonts/$(basename "$FONT")"
    cat > "$FONTDIR/wkfonts.qrc" <<EOF
<!DOCTYPE RCC>
<RCC version="1.0">
    <qresource prefix="/fonts">
        <file>$(basename "$FONT")</file>
    </qresource>
</RCC>
EOF

    # --- wit bindings --------------------------------------------------------
    # Regenerated every build into OUR gen/ rather than gfx-compat/gen: the
    # shared one is disposable and several plugin builds may run at once. Only
    # the *_component_type.o files are used from here — the shims themselves
    # are already objects inside libqwk.a.
    echo "=== wit-bindgen (wkgfx world)"
    wit-bindgen c --world wkgfx "$GFXCOMPAT/wit" --out-dir "$GEN" >/dev/null
    echo "=== wit-bindgen (wkclipboard world)"
    wit-bindgen c --world wkclipboard "$CLIPCOMPAT/wit" --out-dir "$GEN" >/dev/null

    fetch_tar "https://download.kde.org/stable/release-service/$KCALC_VER/src/kcalc-$KCALC_VER.tar.xz" \
              "kcalc-$KCALC_VER"
    apply_patches "$SRCDIR/kcalc-$KCALC_VER" kcalc

    local LOG="$LOGDIR/app.log" BLD="$BUILD/app"
    if [ ! -f "$BLD/build.ninja" ] || [ -n "${WK_KCALC_RECONFIGURE:-}" ]; then
        echo "=== configuring kcalc $KCALC_VER (log: $LOG)"
        env PATH="$BUILD_PATH" cmake -G Ninja -S "$SRCDIR/kcalc-$KCALC_VER" -B "$BLD" \
            -DCMAKE_TOOLCHAIN_FILE="$QTPLUGIN/wasip2.cmake" \
            -DWASI_SDK_PREFIX="$WASI_SDK" \
            -DCMAKE_FIND_ROOT_PATH="$QTBASE_SYSROOT;$SYSROOT;$HOSTPREFIX" \
            -DCMAKE_PREFIX_PATH="$QTBASE_SYSROOT;$SYSROOT;$HOSTPREFIX" \
            -DQT_HOST_PATH="$HOST_QT" \
            -DCMAKE_BUILD_TYPE=Release \
            -DBUILD_TESTING=OFF \
            -DENABLE_PCH=OFF \
            -DUSE_DBUS=OFF \
            -DKF_IGNORE_PLATFORM_CHECK=TRUE \
            -DKF6_HOST_TOOLING="$HOSTTOOLING/lib/cmake" \
            -DWK_ICON_ENGINE_PLUGIN="$SYSROOT/lib/plugins/kiconthemes6/iconengines/libKIconEnginePlugin.a" \
            -DWK_QPA_LIB="$QTBASE_SYSROOT/lib/libqwk.a" \
            -DWK_SVG_PLUGINS="$SYSROOT/plugins/imageformats/libqsvg.a;$SYSROOT/plugins/iconengines/libqsvgicon.a" \
            -DWK_GFX_COMPONENT_TYPE="$GEN/wkgfx_component_type.o" \
            -DWK_CLIP_COMPONENT_TYPE="$GEN/wkclipboard_component_type.o" \
            -DWK_FONTS_QRC="$FONTDIR/wkfonts.qrc" \
            2>&1 | tee "$LOG"
    else
        echo "=== app already configured (WK_KCALC_RECONFIGURE=1 to redo)"
    fi
    echo "=== ninja kcalc"
    env PATH="$BUILD_PATH" cmake --build "$BLD" --parallel "$JOBS" 2>&1 | tee -a "$LOG"

    # wasip2.cmake leaves CMAKE_EXECUTABLE_SUFFIX empty on purpose (Qt's
    # architecture config test depends on it), so the linked artifact has no
    # extension — and wasm-component-ld already made it a COMPONENT at link
    # time. No wasip1 adapter, no `wasm-tools component new`.
    # bin/, not the build root: ECM's KDECMakeSettings.cmake sets
    # CMAKE_RUNTIME_OUTPUT_DIRECTORY to ${CMAKE_BINARY_DIR}/bin for every KDE
    # project, which is not where a plain CMake build would put it.
    cp -f "$BLD/bin/kcalc" "$HERE/kcalc.wasm"
    echo
    ls -l "$HERE/kcalc.wasm"
    echo "built plugins/qt-kcalc/kcalc.wasm"
}

STAGES="${WK_KCALC_STAGES:-ecm zlib libintl qtsvg gmp mpfr mpc kf kconfighost app}"

for stage in $STAGES; do
case "$stage" in

# ---------------------------------------------------------------------------
ecm)
    # extra-cmake-modules is pure CMake — no compilation at all — so it is a
    # HOST install, not a cross build, and it must exist before any framework
    # can be configured.
    fetch_kf extra-cmake-modules
    LOG="$LOGDIR/ecm.log"
    echo "=== extra-cmake-modules $KF_VER -> $HOSTPREFIX (log: $LOG)"
    cmake -S "$SRCDIR/extra-cmake-modules" -B "$BUILD/ecm" \
        -DCMAKE_INSTALL_PREFIX="$HOSTPREFIX" \
        -DBUILD_TESTING=OFF -DBUILD_HTML_DOCS=OFF \
        -DBUILD_MAN_DOCS=OFF -DBUILD_QTHELP_DOCS=OFF 2>&1 | tee "$LOG"
    cmake --build "$BUILD/ecm" --target install 2>&1 | tee -a "$LOG"
    ;;

# ---------------------------------------------------------------------------
zlib)
    # KArchive's CMakeLists does an unconditional find_package(ZLIB) with
    # TYPE REQUIRED. Qt's BUNDLED zlib does not satisfy that — it exports
    # Qt6::BundledZLIB, not the ZLIB:: imported target FindZLIB.cmake makes —
    # so a standalone one is needed. Same 1.3.1 recipe plugins/netsurf uses.
    fetch_tar "https://zlib.net/fossils/zlib-$ZLIB_VER.tar.gz" "zlib-$ZLIB_VER"
    apply_patches "$SRCDIR/zlib-$ZLIB_VER" zlib
    cross_cmake zlib "$SRCDIR/zlib-$ZLIB_VER" -DZLIB_BUILD_EXAMPLES=OFF
    # zlib's CMakeLists says `add_library(zlib SHARED)` literally, so
    # BUILD_SHARED_LIBS=OFF does not stop it, and it installs BOTH
    # libzlib.so and libzlibstatic.a — neither of which is the name
    # CMake's own FindZLIB looks for first. FindZLIB's NAMES list is
    # `z zlib zdll zlib1 zlibstatic`, and CMake tries each name across all
    # directories before moving to the next, so it would find `zlib` — the
    # .so — and hand KArchive a wasm "shared library" that cannot be linked.
    # Rename to the libz.a that zlib's OWN autotools build produces, and
    # delete the .so so the wrong answer is not available at all.
    mv -f "$SYSROOT/lib/libzlibstatic.a" "$SYSROOT/lib/libz.a"
    rm -f "$SYSROOT/lib/libzlib.so"
    ls -l "$SYSROOT/lib/libz.a"
    ;;

# ---------------------------------------------------------------------------
libintl)
    # KI18n does find_package(LibIntl) TYPE REQUIRED, and wasi-sdk has neither
    # <libintl.h> nor dgettext (verified: HAVE_LIBINTL_H and HAVE_DGETTEXT both
    # come back empty under this toolchain).
    #
    # proxy-libintl is upstream's own answer to exactly this: a passthrough
    # gettext that returns the msgid. KDE itself relies on the same trick on
    # Android (see the comment at ki18n's src/i18n/kcatalog.cpp:193).
    #
    # WHAT THIS COSTS, stated plainly: the app is English-only BY
    # CONSTRUCTION, permanently, until someone cross-builds a real
    # gettext-runtime (which wants iconv and locale support wasi-libc barely
    # has). It is NOT a stub of any KDE behaviour — KLocalizedString still
    # runs, still does its argument substitution and plural selection; it just
    # never finds a catalog. The upside is that no .mo files need staging into
    # the node's vfs.
    #
    # Built by hand rather than through its meson build: it is one C file and
    # one header, and adding meson+ninja+a cross file to this port's toolchain
    # surface to compile 200 lines would be the tail wagging the dog.
    PLI="$SRCDIR/proxy-libintl"
    if [ ! -d "$PLI/.git" ]; then
        rm -rf "$PLI"
        echo "cloning proxy-libintl..."
        git clone --quiet --depth 1 https://github.com/frida/proxy-libintl.git "$PLI"
    fi
    echo "=== libintl (passthrough) -> $SYSROOT"
    mkdir -p "$SYSROOT/include" "$SYSROOT/lib"
    # STUB_ONLY=1 and G_INTL_STATIC_COMPILATION: without the first,
    # proxy-libintl dlopen()s a real libintl at startup and only falls back to
    # passthrough when that fails. On wasip2 dlopen is a linkable stub that
    # always returns NULL, so it would work by accident — but "works because
    # the stub fails" is not a thing to rely on, and STUB_ONLY removes the
    # <dlfcn.h> include entirely. The second is only meaningful on Windows;
    # passed so the intent is on the record.
    env PATH="$BUILD_PATH" "$WASI_SDK/bin/clang" --target=wasm32-wasip2 -Os \
        -DSTUB_ONLY=1 -DG_INTL_STATIC_COMPILATION \
        -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN \
        -fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false \
        -I"$PLI" -c "$PLI/libintl.c" -o "$BUILD/libintl.o" 2>&1 | tee "$LOGDIR/libintl.log"
    env PATH="$BUILD_PATH" "$WASI_SDK/bin/llvm-ar" rcs "$SYSROOT/lib/libintl.a" "$BUILD/libintl.o"
    cp -f "$PLI/libintl.h" "$SYSROOT/include/libintl.h"
    ls -l "$SYSROOT/lib/libintl.a"
    ;;

# ---------------------------------------------------------------------------
qtsvg)
    # Qt6::Svg + the qsvg imageformat and iconengine plugins. KIconThemes does
    # find_package(Qt6Svg REQUIRED) at CMakeLists.txt:47 — unconditional — and
    # Breeze's icons are SVG, so this is load-bearing rather than cosmetic.
    # qtsvg is a separate repo, hence layerable into our prefix; Network and
    # PrintSupport are not, which is why those two get patched out instead.
    fetch_tar "https://download.qt.io/archive/qt/$QT_SERIES/$QT_VER/submodules/qtsvg-everywhere-opensource-src-$QT_VER.tar.xz" \
              "qtsvg-everywhere-src-$QT_VER"
    apply_patches "$SRCDIR/qtsvg-everywhere-src-$QT_VER" qtsvg
    # WARNINGS_ARE_ERRORS=OFF: wasi-sdk 34-rc.2's clang reports itself as Clang
    # 23, which Qt 6.8.4 predates, and Qt's developer defaults promote its new
    # diagnostics to errors.
    cross_cmake qtsvg "$SRCDIR/qtsvg-everywhere-src-$QT_VER" \
        -DQT_BUILD_EXAMPLES=OFF -DQT_BUILD_TESTS=OFF \
        -DQT_BUILD_BENCHMARKS=OFF -DWARNINGS_ARE_ERRORS=OFF
    ;;

# ---------------------------------------------------------------------------
gmp|mpfr|mpc)
    # KCalc's knumber/CMakeLists.txt makes GMP, MPFR and MPC all TYPE REQUIRED.
    # These are the only autotools builds in this port and they are entirely
    # orthogonal to the KDE question — which is exactly why the frameworks are
    # proven first: a GMP configure failure must not be misread as evidence
    # that KDE-on-wasm does not work.
    #
    # The wasm32 specifics:
    #   --disable-assembly   GMP ships hand-written asm for every real CPU and
    #                        none for wasm; without this it picks a generic-but-
    #                        still-asm path and fails to assemble.
    #   ABI=32               wasm32 is a 32-bit ABI with a 64-bit long long.
    #   --host=wasm32-wasi   makes configure take the cross path and stop
    #                        trying to RUN its test programs.
    # MPFR and MPC then only need to be pointed at GMP's prefix.
    autotools_build "$stage"
    ;;

# ---------------------------------------------------------------------------
kconfighost)
    # A NATIVE kconfig_compiler, and it is not optional: KCalc's
    # kconfig_add_kcfg_files(kcalc kcalc_settings.kcfgc) generates
    # kcalc_settings.{h,cpp} at BUILD time by RUNNING KF6::kconfig_compiler.
    # The cross build installs a wasm32-wasip2 one, so ninja tries to exec a
    # .wasm and the build dies with the wonderfully opaque
    #     FAILED: [code=126] kcalc_settings.h kcalc_settings.cpp
    # (126 is "cannot execute"). Same class of problem as moc, and Qt already
    # solves its half via QT_HOST_PATH.
    #
    # Upstream anticipates this: KF6ConfigConfig.cmake.in:21 checks
    # `if(CMAKE_CROSSCOMPILING AND KF6_HOST_TOOLING)` and find_file()s
    # KF6Config/KF6ConfigCompilerTargets.cmake under that prefix with
    # NO_DEFAULT_PATH and NO_CMAKE_FIND_ROOT_PATH. So all this stage has to do
    # is produce that file for the host.
    #
    # It goes into its OWN prefix ($HOSTTOOLING), NOT $HOSTPREFIX where ECM
    # lives, and $HOSTTOOLING is deliberately kept OFF the app's
    # CMAKE_PREFIX_PATH. A host KF6ConfigConfig.cmake sitting on the search
    # path next to the wasm one is exactly the sort of thing that resolves in
    # the wrong order six months later and hands a Mach-O .a to a wasm link.
    # KF6_HOST_TOOLING is searched with NO_DEFAULT_PATH precisely so it does
    # not need to be.
    #
    # KCONFIG_USE_GUI/QML=OFF: plugins/qt/host is a tools-only Qt (Core, Xml,
    # the *Tools packages, Qml) with no Qt6Gui and no Qt6Widgets. Turning both
    # off reduces this build to KF6ConfigCore + kconfig_compiler, which need
    # Qt6::Core and Qt6::Xml and nothing else.
    fetch_kf kconfig
    apply_patches_git "$SRCDIR/kconfig" kconfig
    LOG="$LOGDIR/kconfig-host.log"
    echo "=== native kconfig_compiler -> $HOSTTOOLING (log: $LOG)"
    cmake -G Ninja -S "$SRCDIR/kconfig" -B "$BUILD/kconfig-host" \
        -DCMAKE_INSTALL_PREFIX="$HOSTTOOLING" \
        -DCMAKE_PREFIX_PATH="$HOST_QT;$HOSTPREFIX" \
        -DCMAKE_BUILD_TYPE=Release \
        -DBUILD_SHARED_LIBS=OFF \
        -DBUILD_TESTING=OFF \
        -DBUILD_PYTHON_BINDINGS=OFF \
        -DENABLE_PCH=OFF \
        -DUSE_DBUS=OFF \
        -DKCONFIG_USE_GUI=OFF \
        -DKCONFIG_USE_QML=OFF 2>&1 | tee "$LOG"
    cmake --build "$BUILD/kconfig-host" --parallel "$JOBS" 2>&1 | tee -a "$LOG"
    cmake --install "$BUILD/kconfig-host" 2>&1 | tee -a "$LOG"
    ls -l "$HOSTTOOLING/lib/cmake/KF6Config/KF6ConfigCompilerTargets.cmake"
    ;;

# ---------------------------------------------------------------------------
kf)
    for repo in ${WK_KCALC_KF:-$KF_ORDER}; do
        kf_stage "$repo"
    done
    ;;

# ---------------------------------------------------------------------------
app)
    app_stage
    ;;

*)
    echo "qt-kcalc: unknown stage '$stage'" >&2
    exit 1
    ;;
esac
done
