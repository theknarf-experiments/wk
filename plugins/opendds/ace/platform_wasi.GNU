# -*- Makefile -*-
#
# ACE platform macros for wasm32-wasip2, for the wk runtime.
# Installed into $ACE_ROOT/include/makeinclude/ by ../build-target.sh.
#
# This is OUR file, not a patch: ACE's convention is one platform_*.GNU per
# platform, and WASI is a platform ACE does not know about. It is deliberately
# self-contained rather than including platform_linux_common.GNU, which probes
# the build machine (uname, glibc version, kernel version) for facts about the
# TARGET — every one of which would be a lie here.
#
# The flag set is not invented. It is the one plugins/qt/wasip2.cmake
# establishes and plugins/libreoffice, plugins/mupdf and plugins/bun all
# reuse; the WHY for each flag is written out at length in that file's
# "exception handling / setjmp" section and is not repeated here. The short
# version, because getting any of them wrong fails far from the cause:
#
#   -fwasm-exceptions ... -wasm-use-legacy-eh=false
#       wk runs wasmtime with the exnref proposal on and REJECTS wasi-sdk's
#       default legacy EH encoding, at instantiate time — so one translation
#       unit compiled without these poisons the whole component. ACE and TAO
#       both throw. -fwasm-exceptions additionally selects wasi-sdk 34's eh/
#       variant of libc++/libc++abi and puts lib/wasm32-wasip2/eh on the
#       library search path, which is the only reason -lunwind resolves.
#   -mllvm -wasm-enable-sjlj
#       real setjmp/longjmp. TAO_IDL's front end is not in this build, but
#       ACE's ACE_OS::sigsetjmp paths are, and clang errors at codegen on wasm
#       without it.
#   the four _WASI_EMULATED_* defines
#       needed at COMPILE time, not only at link time: wasi-libc's <signal.h>
#       and <sys/mman.h> are #error headers without them, and ACE includes
#       both from ace/OS_NS_signal.h and ace/Mem_Map.h.
#   no LTO
#       LTO re-runs codegen at link time, where the -mllvm flags are not in
#       effect, and the result is a component wasmtime rejects.

WASI_SDK ?= $(error platform_wasi.GNU: WASI_SDK is not set)

CC  := $(WASI_SDK)/bin/clang
CXX := $(WASI_SDK)/bin/clang++
AR  := $(WASI_SDK)/bin/llvm-ar
RANLIB := $(WASI_SDK)/bin/llvm-ranlib
LD  := $(CXX)

# ACE runs `$(CXX) --version` / `-dumpversion` to decide which workarounds to
# apply. Answer from the same compiler that will do the building.
CXX_FOR_VERSION_TEST ?= $(CXX)
ifndef CXX_FULL_VERSION
  CXX_FULL_VERSION := $(shell $(CXX_FOR_VERSION_TEST) --version)
endif
ifndef CXX_VERSION
  CXX_VERSION := $(shell $(CXX_FOR_VERSION_TEST) -dumpversion)
endif

WK_WASI_TARGET := --target=wasm32-wasip2
WK_WASI_EH := -fwasm-exceptions -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false
WK_WASI_EMULATION := -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN \
                     -D_WASI_EMULATED_PROCESS_CLOCKS -D_WASI_EMULATED_GETPID

FLAGS_C_CC += $(WK_WASI_TARGET) $(WK_WASI_EMULATION) $(WK_WASI_EH)

# The port's own headers for what wasi-libc declares nowhere: <net/if.h>
# (struct ifreq/ifconf, which ACE's SIOCGIFCONF paths need) and
# wk-wasi-net-compat.h (struct cmsghdr, the CMSG_* family, and the
# sendmsg/recvmsg prototypes). Both are read in shim/include/, and the -I comes
# AFTER the sysroot, so a future wasi-sdk that ships the real thing shadows
# ours rather than colliding with it. See shim/include/*.h for why these are
# headers and not patches to ACE.
WK_OPENDDS_SHIM ?= $(error platform_wasi.GNU: WK_OPENDDS_SHIM is not set)
FLAGS_C_CC += -I$(WK_OPENDDS_SHIM)/include

# ACE 8 requires C++17; wasi-sdk's clang defaults to a newer standard, but say
# it, so a toolchain bump cannot silently move the language out from under
# TAO's generated code.
CCFLAGS += -std=c++17

# ACE's own warning set minus the ones that fire in thousands on this target
# purely because wasm32's long is 32-bit and ACE's format strings assume LP64.
# Warnings are not errors here; silencing them keeps the real ones findable.
FLAGS_C_CC += -Wno-format -Wno-unused-parameter

LDFLAGS += $(WK_WASI_TARGET) $(WK_WASI_EH)

# Strip the run-path flag OpenDDS's configure writes into platform_macros.GNU
# for every cross build:
#
#   LDFLAGS += -Wl,-rpath-link,$(ACE_ROOT)/lib
#
# It is meaningless here — wasm has no shared objects and no run-time loader —
# and it breaks every executable link twice over, in a way that reads as two
# unrelated faults:
#
#   wasm-ld: error: unknown argument: -rpath-link
#   wasm-ld: error: cannot open .../ACE_wrappers/lib: Is a directory
#
# The second is the first's argument, taken as an input file once the option
# is rejected. This file is included AFTER configure's line, which is what
# makes filtering possible; `wk_comma` exists because a literal comma inside a
# make function call separates arguments.
wk_comma := ,
LDFLAGS := $(filter-out -Wl$(wk_comma)-rpath-link$(wk_comma)%,$(LDFLAGS))
# The runtime halves of the flags and defines above, plus the shadow stack.
# 8 MB rather than the default 64 KB: ACE's and TAO's CDR marshaling and the
# RTPS parameter-list walk both recurse, and overflowing the shadow stack
# presents as an unattributed trap rather than a stack-overflow message.
# Same size plugins/qt and plugins/mupdf settled on.
LIBS += -lunwind -lsetjmp \
        -lwasi-emulated-signal -lwasi-emulated-mman \
        -lwasi-emulated-process-clocks -lwasi-emulated-getpid

# The port's shims, on EVERY executable link in the tree — ../build-shim.sh
# builds the archive, and ../build-target.sh runs it before make.
#
# ACE_SOCK_Dgram references sendmsg and recvmsg, which wasi-libc declares
# nowhere and ../shim/wk-opendds-net.c supplies, so without this every
# executable OpenDDS builds for itself (its examples, its tests, DCPSInfoRepo)
# dies on `undefined symbol: sendmsg`. Our own nodes pass the archive on their
# own link lines, so they would work either way; putting it here is what lets
# `make` finish the tree and leaves upstream's test programs runnable, which is
# how the port gets checked against something other than code we wrote.
#
# --whole-archive is not optional and not tidiness: lld registers archive
# members as lazy symbols and fetches the FIRST archive to offer a name, so the
# threading overrides in wk-opendds-threads.c would lose to libc's weak
# definitions if left lazy. See ../shim/wk-opendds-threads.c, "ARCHIVE ORDER".
LIBS += -Wl,--whole-archive $(WK_OPENDDS_SHIM)/libwkopendds.a -Wl,--no-whole-archive
LDFLAGS += -Wl,-z,stack-size=8388608

# --- static only -------------------------------------------------------------
#
# wasm32-wasip2 has no shared objects and no dlopen. Everything links into the
# node binary. This has a consequence ACE cares about elsewhere: ACE's Service
# Configurator normally finds transports and discovery plugins by dlopen()ing
# them at run time from a .conf file, and it cannot here. OpenDDS's answer is
# its static-link initializers (dds/DCPS/StaticIncludes.h and the
# OpenDDS_Rtps* library initializers), which the demo nodes include directly.
shared_libs      = 0
static_libs      = 1
static_libs_only = 1
versioned_so     = 0
no_hidden_visibility = 1
CPPFLAGS += -DACE_HAS_CUSTOM_EXPORT_MACROS=0

# There is no dynamic loader to link against, and ACE must be told so rather
# than left to discover it: with dynamic_loader on, platform_g++_common.GNU
# adds -ldl and ACE compiles ACE_DLL/ACE_Dynamic_Service against dlopen.
dynamic_loader = 0
CPPFLAGS += -DACE_HAS_DYNAMIC_LINKING=0

# THREADS ON, and this is a decision, not an oversight.
#
# wasm32-wasip2 cannot spawn a thread — wasi-libc's pthread_create is a stub
# returning ENOTSUP. The tempting move is therefore threads=0, which would
# compile ACE's null-synchronisation layer. It does not work: OpenDDS includes
# ace/Condition_Thread_Mutex.h unconditionally (dds/DCPS/ConditionVariable.h:8)
# and builds its whole event machinery on ACE_Thread_Mutex + ACE_Condition, so
# an ACE without them does not get as far as compiling.
#
# So ACE keeps its full pthread layer, every mutex and condition variable is a
# real libc call, and the single-threaded behaviour is supplied UNDER libc by
# ../shim/wk-opendds-threads.c on the link line. That is also where it belongs:
# the policy is one file, not a hundred #ifdefs across two upstream trees.
# See ../PORTING.md, "One thread, and a condition variable that pumps".
threads = 1

# ACE's optimisation defaults are -O3 for both; a DDS node is latency-sensitive
# and this is also the only build, so keep them.
OCFLAGS  ?= -O2
OCCFLAGS ?= -O2

# Nothing to run on the build machine, so no version test of the linker and no
# -Wl,-E probe. Setting GNU_LD keeps platform_g++_common.GNU from shelling out
# to a linker that would answer for the host.
GNU_LD = 1

include $(ACE_ROOT)/include/makeinclude/platform_gcc_clang_common.GNU
