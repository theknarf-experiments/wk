# The wk node wrapper around upstream Slate.
#
# Upstream's CMakeLists.txt is add_subdirectory()'d unchanged (modulo
# patches/), and everything wk-specific is bolted onto the `app` target it
# creates. Doing it this way instead of patching upstream's build files keeps
# patches/ small enough to read and to rebase.
#
# Four things are attached here, and every one of them is load-bearing:
#
#   1. libqwk.a -- the wk QPA plugin from plugins/qt. node/wkslate.cpp names it
#      with Q_IMPORT_PLUGIN; this is what supplies the symbol.
#   2. Qt6::FbSupportPrivate -- QFbScreen/QFbWindow/QFbBackingStore, which ARE
#      the guest-side compositor libqwk.a is built on.
#   3. wkgfx_component_type.o, linked as a raw OBJECT and not through an
#      archive. It carries only a `component-type` custom section that
#      wasm-component-ld reads at link time to build the component's imports.
#      Nothing references it, so as an archive member the linker drops it and
#      you get a component with no wasi:surface import at all -- which fails
#      much later, at instantiation, as a missing import.
#   4. a fallback font as a Qt resource under :/wkfonts. See node/wkslate.cpp
#      for why it is a directory of its own and not Slate's :/fonts.
cmake_minimum_required(VERSION 3.21)
project(wk-slate LANGUAGES C CXX)

set(SLATE_SRC "" CACHE PATH "the extracted upstream Slate tree")
set(WK_GFX_COMPAT "" CACHE PATH "plugins/gfx-compat, the wasi-gfx C shim")
set(WK_GEN_DIR "" CACHE PATH "this port's gen/, where ../build.sh writes the bindings")
set(WK_QPA_LIB "" CACHE FILEPATH "plugins/qt/sysroot/lib/libqwk.a")
set(WK_FONT "" CACHE FILEPATH "a .ttf staged into node/fonts/ by ../build.sh")

foreach(v SLATE_SRC WK_GFX_COMPAT WK_GEN_DIR WK_QPA_LIB WK_FONT)
    if(NOT ${v})
        message(FATAL_ERROR "wk-slate: -D${v}=... is required (see ../build.sh)")
    endif()
endforeach()

find_package(Qt6 6.8 REQUIRED COMPONENTS Core Gui Widgets Qml Quick QuickControls2 FbSupportPrivate)

# Upstream defaults ENABLE_TESTING to ON, and tests/ needs Qt::Test, which the
# cross qtbase does not have (FEATURE_testlib=OFF -- QTestLib wants threads and
# a process launcher). FORCE because upstream's option() would otherwise win.
set(ENABLE_TESTING OFF CACHE BOOL "no QtTest in the wasm sysroot" FORCE)

# Upstream reads this out of git; there is no git in a tarball build.
# patches/slate-0001 turns its FATAL_ERROR into this variable.
if(NOT BUILD_VERSION)
    set(BUILD_VERSION "e5cf5b6")
endif()

add_subdirectory("${SLATE_SRC}" slate-upstream)

if(NOT TARGET app)
    message(FATAL_ERROR "wk-slate: upstream did not create the `app` target")
endif()

target_sources(app PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/wkslate.cpp")

qt_add_resources(app "wkfonts"
    PREFIX "/wkfonts"
    BASE "${CMAKE_CURRENT_SOURCE_DIR}/fonts"
    FILES "${WK_FONT}"
)

target_link_libraries(app PRIVATE "${WK_QPA_LIB}" Qt6::FbSupportPrivate)

# THE QML MODULE'S PLUGIN, not just its backing library.
#
# qt_add_qml_module(slate URI "Slate") builds TWO targets: `slate`, the C++
# library, and `slateplugin`, a QQmlEngineExtensionPlugin whose only job is to
# call qml_register_types_Slate() and pull in the compiled QML resource.
# Upstream's app/CMakeLists.txt links only `slate`, which is right on a shared
# Qt -- the plugin is a .so found on the QML import path at runtime.
#
# There is no dlopen here, so nothing ever loads it, and the failure is a long
# way from the cause: the app starts, the wk QPA plugin loads, QtQuick,
# Controls and Layouts all resolve, and then
#
#     qrc:/qml/main.qml:28:1: module "Slate" is not installed
#     QQmlApplicationEngine failed to load component
#
# and main() returns 1 because rootObjects() is empty. Linking the plugin
# target brings in Qt's generated slateplugin_init.cpp, which is a one-line
# Q_IMPORT_PLUGIN(SlatePlugin).
target_link_libraries(app PRIVATE slateplugin)
target_link_options(app PRIVATE "${WK_GFX_COMPAT}/gen/wkgfx_component_type.o")
# The clipboard world's component-type object. Needed even though Slate never
# calls wkclip_*: libqwk.a contains QWkClipboard and the shim it uses, so the
# shim is always pulled out of the archive, and the wit-bindgen output
# references a symbol that lives ONLY in this object. Omit it and the link
# fails with "undefined symbol:
# __component_type_object_force_link_wkclipboard".
target_link_options(app PRIVATE "${WK_GEN_DIR}/wkclipboard_component_type.o")
