# qt-net: the QSocketNotifier test asset. See main.cpp for what it proves.
#
# Smaller than ../smoke in two ways that are deliberate, not laziness:
#
#   * Gui, not Widgets. The event dispatcher under test comes from the QPA
#     plugin, so a GUI application is required — but no widget is, because
#     this node never paints;
#   * no font resource. qt-smoke compiles one in because a Qt app with no font
#     renders every string as nothing; this one renders nothing at all, and a
#     staged font would only make the artifact bigger and the build
#     machine-dependent.
#
# Everything else is qt-smoke's recipe: link libqwk.a, write Q_IMPORT_PLUGIN in
# main.cpp, and link the component-type objects DIRECTLY (each carries only a
# `component-type` custom section that wasm-component-ld reads at link time;
# nothing references them, so an archive member would be dropped and the
# result would be a component missing that import entirely).
#
# wkclipboard_component_type.o is here even though this node never copies or
# pastes: libqwk.a contains QWkClipboard, the QPA plugin's clipboard() returns
# it, so the shim is always pulled out of the archive. Omitting the object is
# a hard undefined-symbol error at link, not a silently broken component.
cmake_minimum_required(VERSION 3.21)
project(qt-net LANGUAGES C CXX)

find_package(Qt6 6.8 REQUIRED COMPONENTS Core Gui FbSupportPrivate)

set(CMAKE_AUTOMOC ON)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

set(WK_GFX_COMPAT "${CMAKE_CURRENT_SOURCE_DIR}/../../gfx-compat" CACHE PATH
    "plugins/gfx-compat, the wasi-gfx C shim")
set(WK_CLIP_COMPAT "${CMAKE_CURRENT_SOURCE_DIR}/../../clipboard-compat" CACHE PATH
    "plugins/clipboard-compat, the wk:clipboard C shim")
set(WK_QPA_LIB "${CMAKE_CURRENT_SOURCE_DIR}/../sysroot/lib/libqwk.a" CACHE FILEPATH
    "the wk QPA plugin archive built by ../build-qpa.sh")

qt_add_executable(qt-net main.cpp)

target_link_libraries(qt-net PRIVATE
    "${WK_QPA_LIB}"
    Qt6::Gui Qt6::Core Qt6::FbSupportPrivate
)

target_link_options(qt-net PRIVATE
    "${WK_GFX_COMPAT}/gen/wkgfx_component_type.o"
    "${WK_CLIP_COMPAT}/gen/wkclipboard_component_type.o"
)
