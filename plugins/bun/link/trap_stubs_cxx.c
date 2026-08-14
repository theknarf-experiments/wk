// Trap stubs for 5 C++ symbols off the 'bun run' startup path (mangled asm-labels):
//  v8::shim::GlobalInternals::{create,s_info} — LazyProperty, only on .node addon load
//  Bun::{setupJSWebViewClassStructure,toJS(WebViewEventTarget)} — Bun.WebView (native backend)
//  WebCore::ScriptWrappable::wrapper() — DOM wrapper slot, unused without a WebCore DOM object
#include <stdint.h>
static const uintptr_t gi_s_info[64] = {0};
extern const void* gi_s_info_a __asm__("_ZN2v84shim15GlobalInternals6s_infoE") __attribute__((alias("gi_s_info")));
void s0(void) __asm__("_ZN2v84shim15GlobalInternals6createERN3JSC2VMEPNS2_9StructureEPN3Zig12GlobalObjectE");
void s0(void){__builtin_trap();}
void s1(void) __asm__("_ZN3Bun28setupJSWebViewClassStructureERN3JSC18LazyClassStructure11InitializerE");
void s1(void){__builtin_trap();}
void s2(void) __asm__("_ZN3Bun4toJSEPN3JSC14JSGlobalObjectEPN3Zig12GlobalObjectERNS_18WebViewEventTargetE");
void s2(void){__builtin_trap();}
void s3(void) __asm__("_ZNK7WebCore15ScriptWrappable7wrapperEv");
void s3(void){__builtin_trap();}
