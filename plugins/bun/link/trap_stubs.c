// Trap stubs for symbols off the 'bun run' startup path:
//  - v8 shim (LazyProperty, only instantiated when a .node addon loads; no dlopen on wasm)
//  - WebView/CDP (native host backend; Bun.WebView API inert here)
//  - libwebp (image codec, inert until decode)
//  - --inspect / test-reporter / devserver agents (gated on features off during bun run)
// Each is a trapping thunk; reaching one is a bug we'd see as unreachable, not silent corruption.
#include <stdint.h>
void f1(void) __asm__("BakeGlobalObject__getPerThreadData");
void f1(void){__builtin_trap();}
void f2(void) __asm__("BakeGlobalObject__isBakeGlobalObject");
void f2(void){__builtin_trap();}
void f3(void) __asm__("Bun__Chrome__died");
void f3(void){__builtin_trap();}
void f4(void) __asm__("Bun__HTTPServerAgent__notifyServerRoutesUpdated");
void f4(void){__builtin_trap();}
void f5(void) __asm__("Bun__HTTPServerAgent__notifyServerStarted");
void f5(void){__builtin_trap();}
void f6(void) __asm__("Bun__HTTPServerAgent__notifyServerStopped");
void f6(void){__builtin_trap();}
void f7(void) __asm__("Bun__LifecycleAgentReportError");
void f7(void){__builtin_trap();}
void f8(void) __asm__("Bun__TestReporterAgentReportTestEnd");
void f8(void){__builtin_trap();}
void f9(void) __asm__("Bun__TestReporterAgentReportTestFound");
void f9(void){__builtin_trap();}
void f10(void) __asm__("Bun__TestReporterAgentReportTestStart");
void f10(void){__builtin_trap();}
void f11(void) __asm__("Bun__WebView__closeAllForTermination");
void f11(void){__builtin_trap();}
void f12(void) __asm__("Bun__WebViewHost__childDied");
void f12(void){__builtin_trap();}
void f13(void) __asm__("Bun::setupJSWebViewClassStructure(JSC::LazyClassStructure::Initializer&)");
void f13(void){__builtin_trap();}
void f14(void) __asm__("Bun::toJS(JSC::JSGlobalObject*, Zig::GlobalObject*, Bun::WebViewEventTarget&)");
void f14(void){__builtin_trap();}
void f15(void) __asm__("InspectorBunFrontendDevServerAgent__notifyBundleComplete");
void f15(void){__builtin_trap();}
void f16(void) __asm__("InspectorBunFrontendDevServerAgent__notifyBundleFailed");
void f16(void){__builtin_trap();}
void f17(void) __asm__("InspectorBunFrontendDevServerAgent__notifyBundleStart");
void f17(void){__builtin_trap();}
void f18(void) __asm__("InspectorBunFrontendDevServerAgent__notifyClientConnected");
void f18(void){__builtin_trap();}
void f19(void) __asm__("InspectorBunFrontendDevServerAgent__notifyClientDisconnected");
void f19(void){__builtin_trap();}
void f20(void) __asm__("InspectorBunFrontendDevServerAgent__notifyClientNavigated");
void f20(void){__builtin_trap();}
void f21(void) __asm__("InspectorBunFrontendDevServerAgent__notifyConsoleLog");
void f21(void){__builtin_trap();}
void f22(void) __asm__("v8::shim::GlobalInternals::create(JSC::VM&, JSC::Structure*, Zig::GlobalObject*)");
void f22(void){__builtin_trap();}
static const uintptr_t d23[64]={0};
extern const void* a23 __asm__("v8::shim::GlobalInternals::s_info") __attribute__((alias("d23")));
void f24(void) __asm__("WebCore::ScriptWrappable::wrapper() const");
void f24(void){__builtin_trap();}
void f25(void) __asm__("WebPDecodeRGBA");
void f25(void){__builtin_trap();}
void f26(void) __asm__("WebPDemuxDelete");
void f26(void){__builtin_trap();}
void f27(void) __asm__("WebPDemuxGetChunk");
void f27(void){__builtin_trap();}
void f28(void) __asm__("WebPDemuxGetI");
void f28(void){__builtin_trap();}
void f29(void) __asm__("WebPDemuxInternal");
void f29(void){__builtin_trap();}
void f30(void) __asm__("WebPDemuxReleaseChunkIterator");
void f30(void){__builtin_trap();}
void f31(void) __asm__("WebPEncodeLosslessRGBA");
void f31(void){__builtin_trap();}
void f32(void) __asm__("WebPEncodeRGBA");
void f32(void){__builtin_trap();}
void f33(void) __asm__("WebPFree");
void f33(void){__builtin_trap();}
void f34(void) __asm__("WebPGetInfo");
void f34(void){__builtin_trap();}
void f35(void) __asm__("WebPMuxAssemble");
void f35(void){__builtin_trap();}
void f36(void) __asm__("WebPMuxDelete");
void f36(void){__builtin_trap();}
void f37(void) __asm__("WebPMuxSetChunk");
void f37(void){__builtin_trap();}
void f38(void) __asm__("WebPMuxSetImage");
void f38(void){__builtin_trap();}
void f39(void) __asm__("WebPNewInternal");
void f39(void){__builtin_trap();}
void f40(void) __asm__("WTFTimer__fire");
void f40(void){__builtin_trap();}
