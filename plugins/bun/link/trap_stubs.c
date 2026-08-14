// Trap stubs for symbols off the 'bun run' path (v8 lazy shim, WebP, agents, Bake).
#include <stdint.h>
void f1(void) __asm__("BakeGlobalObject__getPerThreadData"); void f1(void){__builtin_trap();}
void f2(void) __asm__("BakeGlobalObject__isBakeGlobalObject"); void f2(void){__builtin_trap();}
void f3(void) __asm__("Bun__Chrome__died"); void f3(void){__builtin_trap();}
void f4(void) __asm__("Bun__HTTPServerAgent__notifyServerRoutesUpdated"); void f4(void){__builtin_trap();}
void f5(void) __asm__("Bun__HTTPServerAgent__notifyServerStarted"); void f5(void){__builtin_trap();}
void f6(void) __asm__("Bun__HTTPServerAgent__notifyServerStopped"); void f6(void){__builtin_trap();}
void f7(void) __asm__("Bun__LifecycleAgentReportError"); void f7(void){__builtin_trap();}
void f8(void) __asm__("Bun__TestReporterAgentReportTestEnd"); void f8(void){__builtin_trap();}
void f9(void) __asm__("Bun__TestReporterAgentReportTestFound"); void f9(void){__builtin_trap();}
void f10(void) __asm__("Bun__TestReporterAgentReportTestStart"); void f10(void){__builtin_trap();}
void f11(void) __asm__("Bun::setupJSWebViewClassStructure(JSC::LazyClassStructure::Initializer&)"); void f11(void){__builtin_trap();}
void f12(void) __asm__("Bun::toJS(JSC::JSGlobalObject*, Zig::GlobalObject*, Bun::WebViewEventTarget&)"); void f12(void){__builtin_trap();}
void f13(void) __asm__("InspectorBunFrontendDevServerAgent__notifyBundleComplete"); void f13(void){__builtin_trap();}
void f14(void) __asm__("InspectorBunFrontendDevServerAgent__notifyBundleFailed"); void f14(void){__builtin_trap();}
void f15(void) __asm__("InspectorBunFrontendDevServerAgent__notifyBundleStart"); void f15(void){__builtin_trap();}
void f16(void) __asm__("InspectorBunFrontendDevServerAgent__notifyClientConnected"); void f16(void){__builtin_trap();}
void f17(void) __asm__("InspectorBunFrontendDevServerAgent__notifyClientDisconnected"); void f17(void){__builtin_trap();}
void f18(void) __asm__("InspectorBunFrontendDevServerAgent__notifyClientNavigated"); void f18(void){__builtin_trap();}
void f19(void) __asm__("InspectorBunFrontendDevServerAgent__notifyConsoleLog"); void f19(void){__builtin_trap();}
void f20(void) __asm__("v8::shim::GlobalInternals::create(JSC::VM&, JSC::Structure*, Zig::GlobalObject*)"); void f20(void){__builtin_trap();}
static const uintptr_t d21[64]={0};
extern const void* a21 __asm__("v8::shim::GlobalInternals::s_info") __attribute__((alias("d21")));
void f22(void) __asm__("WebCore::ScriptWrappable::wrapper() const"); void f22(void){__builtin_trap();}
void f23(void) __asm__("WebPDecodeRGBA"); void f23(void){__builtin_trap();}
void f24(void) __asm__("WebPDemuxDelete"); void f24(void){__builtin_trap();}
void f25(void) __asm__("WebPDemuxGetChunk"); void f25(void){__builtin_trap();}
void f26(void) __asm__("WebPDemuxGetI"); void f26(void){__builtin_trap();}
void f27(void) __asm__("WebPDemuxInternal"); void f27(void){__builtin_trap();}
void f28(void) __asm__("WebPDemuxReleaseChunkIterator"); void f28(void){__builtin_trap();}
void f29(void) __asm__("WebPEncodeLosslessRGBA"); void f29(void){__builtin_trap();}
void f30(void) __asm__("WebPEncodeRGBA"); void f30(void){__builtin_trap();}
void f31(void) __asm__("WebPFree"); void f31(void){__builtin_trap();}
void f32(void) __asm__("WebPGetInfo"); void f32(void){__builtin_trap();}
void f33(void) __asm__("WebPMuxAssemble"); void f33(void){__builtin_trap();}
void f34(void) __asm__("WebPMuxDelete"); void f34(void){__builtin_trap();}
void f35(void) __asm__("WebPMuxSetChunk"); void f35(void){__builtin_trap();}
void f36(void) __asm__("WebPMuxSetImage"); void f36(void){__builtin_trap();}
void f37(void) __asm__("WebPNewInternal"); void f37(void){__builtin_trap();}
void f38(void) __asm__("WTFTimer__fire"); void f38(void){__builtin_trap();}
