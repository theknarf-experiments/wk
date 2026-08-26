# qt-qtnetwork: QtNetwork over wk's fabric. See main.cpp for what it proves.
#
# The same recipe as ../net, plus Qt6::Network — which only exists in the
# sysroot since build-qtbase.sh flipped FEATURE_network ON. Gui (not Widgets,
# not Core alone) for the same reason ../net gives: the event dispatcher under
# test is installed by the wk QPA plugin, so the application object has to be a
# QGuiApplication, but nothing here paints, so no widget and no font resource.
#
# The component-type objects are link options rather than libraries on purpose:
# each carries only a `component-type` custom section that wasm-component-ld
# reads at link time, nothing references them, and an archive member that
# nothing references is dropped — producing a component silently missing that
# import. wkclipboard is here even though this node never copies: libqwk.a
# contains QWkClipboard and the QPA plugin's clipboard() returns it, so the
# shim is always pulled out of the archive.
cmake_minimum_required(VERSION 3.21)
project(qt-qtnetwork LANGUAGES C CXX)

find_package(Qt6 6.8 REQUIRED COMPONENTS Core Gui Network FbSupportPrivate)

set(CMAKE_AUTOMOC ON)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

set(WK_GFX_COMPAT "${CMAKE_CURRENT_SOURCE_DIR}/../../gfx-compat" CACHE PATH
    "plugins/gfx-compat, the wasi-gfx C shim")
set(WK_CLIP_COMPAT "${CMAKE_CURRENT_SOURCE_DIR}/../../clipboard-compat" CACHE PATH
    "plugins/clipboard-compat, the wk:clipboard C shim")
set(WK_QPA_LIB "${CMAKE_CURRENT_SOURCE_DIR}/../sysroot/lib/libqwk.a" CACHE FILEPATH
    "the wk QPA plugin archive built by ../build-qpa.sh")

qt_add_executable(qt-qtnetwork main.cpp)

target_link_libraries(qt-qtnetwork PRIVATE
    "${WK_QPA_LIB}"
    Qt6::Network Qt6::Gui Qt6::Core Qt6::FbSupportPrivate
)

target_link_options(qt-qtnetwork PRIVATE
    "${WK_GFX_COMPAT}/gen/wkgfx_component_type.o"
    "${WK_CLIP_COMPAT}/gen/wkclipboard_component_type.o"
)
