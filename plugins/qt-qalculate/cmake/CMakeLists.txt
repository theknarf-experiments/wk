# qalculate-qt 5.12.0 — a CMake build, written for the wk port.
#
# THIS FILE IS NOT UPSTREAM'S AND IS NOT A PATCH TO UPSTREAM'S. qalculate-qt
# v5.12.0 has no CMake build system at all: the only build description in the
# tarball is `qalculate-qt.pro`, and wk's Qt is a genuine CMake `WASI`
# platform with no qmake mkspec (see plugins/qt/PORTING.md). So build.sh
# copies this file over the fetched tree, and the port owns it.
#
# It is a translation of qalculate-qt.pro and nothing more. Everything it does
# differently from the .pro is listed here, so a version bump can be checked
# against this list rather than against a diff:
#
#   * `QT += network` is DROPPED. There are zero QNetwork* uses in the whole
#     of src/; the only QtNetwork classes are QLocalSocket/QLocalServer for
#     the single-instance handshake, which patches/qalculate-qt-0002 compiles
#     out (wk's Qt has QT_FEATURE_localsocket off and does not even install
#     those headers). See that patch for why the feature is meaningless for a
#     node.
#   * Qt6::Svg is ADDED, together with the two static qsvg plugins, because
#     icons.qrc is 53 SVGs and there is no dlopen on wasm.
#   * translations.qrc, the lrelease step and all the INSTALLS rules are
#     dropped: there is no `make install` for a node, and Qt LinguistTools is
#     not built in plugins/qt/host.
#   * the source list is GLOBBED rather than enumerated, and build.sh asserts
#     that the glob still equals the .pro's SOURCES. That is deliberate: the
#     failure mode of hand-copying a list into a fork is a silently dropped
#     translation unit on the next upstream bump, and a check is cheaper than
#     vigilance.
#
# The four WK_* cache variables and QALC_PREFIX are all set by build.sh.

cmake_minimum_required(VERSION 3.21)
project(qalculate-qt LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)
set(CMAKE_AUTORCC OFF)          # the .qrc files are named explicitly below

find_package(Qt6 6.8 REQUIRED COMPONENTS Core Gui Widgets)
# Qt6Svg by its OWN package name and NOT as a component of the Qt6 umbrella.
# Svg is a real dependency here (see the header) but it lives in THIS port's
# ./sysroot while Qt6Config.cmake lives in plugins/qt/sysroot, and the umbrella
# searches for its components with NO_DEFAULT_PATH against a path list derived
# from its own location -- so `COMPONENTS Svg` fails with "Expected Config file
# at <qtbase prefix>/lib/cmake/Qt6Svg/Qt6SvgConfig.cmake does NOT exist" while
# the module sits right there in the other prefix. A direct find_package()
# goes through the ordinary CMAKE_PREFIX_PATH search and finds it.
# (plugins/qt-torrentfileeditor103 hits the same wall and upstream's own
# CMakeLists happens to be written this way already.)
find_package(Qt6Svg 6.8 REQUIRED)

# --- libqalculate and its three C dependencies ------------------------------
# Absolute archive paths rather than find_library(): they are built by this
# port's own build.sh into a prefix it knows, there is no system copy that
# could ever be right for wasm32-wasip2, and a find_library() that fell back
# to the host's /opt/homebrew/lib would produce a link error 40 lines deep
# instead of a missing-file error here.
#
# ORDER MATTERS with a static linker: libqalculate calls libxml2, mpfr and
# gmp; mpfr calls gmp. Reversing them gives pages of undefined symbols.
set(QALC_PREFIX "" CACHE PATH "prefix holding libqalculate/libxml2/libmpfr/libgmp for wasm32-wasip2")
if(NOT QALC_PREFIX)
  message(FATAL_ERROR "pass -DQALC_PREFIX=<this port's ./sysroot> (build.sh does)")
endif()
foreach(lib qalculate xml2 mpfr gmp)
  if(NOT EXISTS "${QALC_PREFIX}/lib/lib${lib}.a")
    message(FATAL_ERROR "no lib${lib}.a in ${QALC_PREFIX}/lib -- run ./build.sh's `deps` stage")
  endif()
  list(APPEND QALC_LIBS "${QALC_PREFIX}/lib/lib${lib}.a")
endforeach()

# --- sources ----------------------------------------------------------------
file(GLOB QALC_SOURCES CONFIGURE_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/src/*.cpp")
list(SORT QALC_SOURCES)

set(QALC_RESOURCES
  "${CMAKE_CURRENT_SOURCE_DIR}/icons.qrc"
  "${CMAKE_CURRENT_SOURCE_DIR}/flags.qrc"
)

# --- wk node: fonts ---------------------------------------------------------
# Qt 6 ships no fonts and a wk node has no host font directory: with none, the
# app runs, QFontDatabase is empty and every string renders as nothing at all.
# build.sh stages one TTF and generates this .qrc beside it (which font exists
# depends on the machine); the wk QPA plugin's font database falls back to
# :/fonts after looking in QT_QPA_FONTDIR and the usual VFS directories.
set(WK_FONTS_QRC "" CACHE FILEPATH "generated .qrc holding one staged TTF under :/fonts")
if(WK_FONTS_QRC)
  list(APPEND QALC_RESOURCES "${WK_FONTS_QRC}")
endif()

qt6_add_resources(QALC_RESOURCE_SOURCES ${QALC_RESOURCES})

add_executable(qalculate-qt ${QALC_SOURCES} ${QALC_RESOURCE_SOURCES})

target_include_directories(qalculate-qt PRIVATE
  "${CMAKE_CURRENT_SOURCE_DIR}/src"
  "${QALC_PREFIX}/include"
  "${QALC_PREFIX}/include/libxml2"     # libxml2 installs its headers one level
                                       # deeper; libqalculate's own headers
                                       # include <libxml/parser.h> unprefixed
)

target_link_libraries(qalculate-qt PRIVATE
  Qt6::Widgets Qt6::Gui Qt6::Core Qt6::Svg
  ${QALC_LIBS}
)

# --- wk node: the platform plugin and the component's imports ---------------
# There is no dlopen on wasm, so every Qt plugin is an ARCHIVE linked into the
# binary and named once with Q_IMPORT_PLUGIN (see the patch to src/main.cpp).
#
#   WK_QPA_LIB             plugins/qt/sysroot/lib/libqwk.a -- the wk QPA
#                          plugin. It also carries the gfx-compat and
#                          clipboard-compat shim objects, so the app needs no
#                          other wasi-gfx code.
#   WK_SVG_PLUGINS         the qsvg imageformat + iconengine plugins.
#   WK_*_COMPONENT_TYPE    passed as LINK OPTIONS and never as libraries. Each
#                          holds only a `component-type` custom section that
#                          wasm-component-ld reads to build the component's
#                          import list; nothing references them, so as archive
#                          members the linker drops them and the result is a
#                          component with NO wasi:surface import at all.
set(WK_QPA_LIB "" CACHE FILEPATH "libqwk.a, the wk QPA plugin (plugins/qt/build-qpa.sh)")
set(WK_SVG_PLUGINS "" CACHE STRING "static qsvg plugin archives")
set(WK_GFX_COMPONENT_TYPE "" CACHE FILEPATH "gfx-compat gen/wkgfx_component_type.o")
set(WK_CLIP_COMPONENT_TYPE "" CACHE FILEPATH "clipboard-compat gen/wkclipboard_component_type.o")

if(WK_QPA_LIB)
  # FbSupportPrivate is not optional and not obvious: the wk QPA plugin IS
  # fbconvenience -- QWkScreen derives from QFbScreen, QWkWindow from
  # QFbWindow, QWkBackingStore from QFbBackingStore, and that is where the
  # guest-side compositing of N top-level windows into one surface lives.
  # Without it the link fails with a page of "undefined symbol: QFbScreen::..."
  # from inside libqwk.a. It is an INTERNAL_MODULE, so it is its own package
  # rather than a component of Qt6Gui.
  find_package(Qt6 6.8 REQUIRED COMPONENTS FbSupportPrivate)
  target_link_libraries(qalculate-qt PRIVATE
    ${WK_SVG_PLUGINS} "${WK_QPA_LIB}" Qt6::FbSupportPrivate)
endif()
foreach(obj WK_GFX_COMPONENT_TYPE WK_CLIP_COMPONENT_TYPE)
  if(${obj})
    target_link_options(qalculate-qt PRIVATE "${${obj}}")
  endif()
endforeach()
