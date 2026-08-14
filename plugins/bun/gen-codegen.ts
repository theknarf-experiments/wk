// Runs the byte-class table generators out of the cached bun checkout with a
// minimal Config stand-in — the wk build never runs `bun bd --configure-only`.
// The tables are target-independent; build_options.rs is hand-maintained
// alongside this script (it needs values the generators don't produce).
import { generateJsonByteClass } from "./bun/scripts/build/jsonByteClass.ts";
import { generateXmlByteClass } from "./bun/scripts/build/xmlByteClass.ts";
const cfg = { codegenDir: import.meta.dir + "/codegen" } as any;
generateJsonByteClass(cfg);
generateXmlByteClass(cfg);

// build_options.rs needs values the generators can't produce without a full
// `Config` (sha/version), so it's templated here instead of importing
// buildOptionsRs.ts.
await Bun.write(
  cfg.codegenDir + "/build_options.rs",
  `// Hand-written stand-in for the file \`scripts/build/buildOptionsRs.ts\`
// generates at \`bun bd\` configure time — the wk plugin build drives cargo
// directly (BUN_CODEGEN_DIR points here) and never runs bun's configure.
// Mirrors the generator's output shape; version values match the source
// checkout (package.json / nodejs-headers.ts) and go stale only if the
// cached bun source under bun/ is re-fetched at a new revision.
// \`target_os = "wasi"\` is added to the tinycc opt-out: tinycc is a JIT and
// can never build on wasm.
#[allow(dead_code, unreachable_pub, unused)]
pub const SHA: &str = "b7a0431032129d74fa4a7e3704eaf57b92fa9136";
#[allow(dead_code, unreachable_pub, unused)]
pub const REPORTED_NODEJS_VERSION: &str = "26.3.0";
#[allow(dead_code, unreachable_pub, unused)]
pub const RELEASE_SAFE: bool = true;
#[allow(dead_code, unreachable_pub, unused)]
pub const IS_CANARY: bool = false;
#[allow(dead_code, unreachable_pub, unused)]
pub const CANARY_REVISION: &str = "";
#[allow(dead_code, unreachable_pub, unused)]
pub const ENABLE_FUZZILLI: bool = false;
#[allow(dead_code, unreachable_pub, unused)]
pub const FALLBACK_HTML_VERSION: &str = "0000000000000000";
pub const VERSION: crate::Version = crate::Version {
    major: 1,
    minor: 4,
    patch: 0,
};
#[allow(dead_code, unreachable_pub, unused)]
pub const BASE_PATH: &[u8] = b"";
#[allow(dead_code, unreachable_pub, unused)]
pub const CODEGEN_PATH: &[u8] = b"";
#[allow(dead_code, unreachable_pub, unused)]
pub const ENABLE_LOGS: bool = cfg!(bun_debug);
#[allow(dead_code, unreachable_pub, unused)]
pub const ENABLE_ASAN: bool = cfg!(bun_asan);
#[allow(dead_code, unreachable_pub, unused)]
pub const ENABLE_TINYCC: bool = !cfg!(any(
    target_os = "android",
    target_os = "freebsd",
    target_os = "wasi",
));
`,
);
// The *_jsc tier's build.rs artifacts. cppbind scans every .cpp for
// [[ZIG_EXPORT]]; bundle-modules bundles the JS builtins (TARGET_PLATFORM is
// a JS-visible `process.platform` string — wasi isn't one, linux is closest);
// generate-node-errors emits ErrorCode.generated.rs.
import { $ } from "bun";
const dir = import.meta.dir;
await $`bash -c ${"cd " + dir + "/bun && find src packages -name '*.cpp' | grep -vE 'test|Test' > " + dir + "/codegen/cxx-sources.txt"}`;
await $`bun ${dir}/bun/src/codegen/cppbind.ts src ${dir}/codegen/cpp.rs ${dir}/codegen/cxx-sources.txt`.cwd(dir + "/bun");
await $`bash -c ${"mkdir -p /tmp/bun-codegen-root"}`;
await $`env TARGET_PLATFORM=linux TARGET_ARCH=x64 bun ${dir}/bun/src/codegen/bundle-modules.ts --debug=OFF /tmp/bun-codegen-root`.cwd(dir + "/bun");
await $`bash -c ${"cp -r /tmp/bun-codegen-root/codegen/* " + dir + "/codegen/"}`;
await $`bun ${dir}/bun/src/codegen/generate-node-errors.ts ${dir}/codegen`.cwd(dir + "/bun");
// bun_runtime's build.rs artifacts: the .classes.ts bindings, stream sinks,
// and host exports.
const classFiles = (await $`bash -c ${"find src -name '*.classes.ts'"}`.cwd(dir + "/bun").text()).trim().split("\n");
await $`bun ${dir}/bun/src/codegen/generate-classes.ts ${classFiles} ${dir}/codegen`.cwd(dir + "/bun");
await $`bun ${dir}/bun/src/codegen/generate-jssink.ts ${dir}/codegen`.cwd(dir + "/bun");
await $`bun ${dir}/bun/src/codegen/generate-host-exports.ts ${dir}/codegen`.cwd(dir + "/bun");
// The C++ bindings' generated headers (for the wasi C++ compile, not the
// Rust build). bindgen emits GeneratedBunObject.h + GeneratedBindings.cpp;
// create-hash-table.ts turns the `@begin XXXTable` blocks into .lut.h (it
// strips the `namespace JSC` wrapper the raw perl adds — the .cpp reference
// the tables unqualified). TARGET_PLATFORM=linux matches the runtime codegen.
await $`bun ${dir}/bun/src/codegen/bindgen.ts --codegen-root=${dir}/codegen`.cwd(dir + "/bun");
// bindgenv2 is a *separate* generator (src/codegen/bindgenv2/script.ts) driven
// off `.bindv2.ts` option-structs (SSLConfig, SocketConfig, FakeTimersConfig).
// It emits the `bindgenConvertJSTo<Name>` C++ shims that `src/jsc/generated.rs`
// declares extern — without it those symbols are undefined and every
// Bun.serve/listen/connect config parse traps. Sources must be absolute.
const bindv2Sources = (await $`bash -c ${"find " + dir + "/bun/src -name '*.bindv2.ts'"}`.text())
  .trim()
  .split("\n")
  .join(",");
await $`bun ${dir}/bun/src/codegen/bindgenv2/script.ts --command=generate --sources=${bindv2Sources} --codegen-path=${dir}/codegen`.cwd(dir + "/bun");
const lutPairs: [string, string][] = [
  ["src/jsc/bindings/BunObject.cpp", "BunObject.lut.h"],
  ["src/jsc/bindings/ZigGlobalObject.lut.txt", "ZigGlobalObject.lut.h"],
  ["src/jsc/bindings/JSBuffer.cpp", "JSBuffer.lut.h"],
  ["src/jsc/bindings/BunProcess.cpp", "BunProcess.lut.h"],
  ["src/jsc/bindings/ProcessBindingBuffer.cpp", "ProcessBindingBuffer.lut.h"],
  ["src/jsc/bindings/ProcessBindingConstants.cpp", "ProcessBindingConstants.lut.h"],
  ["src/jsc/bindings/ProcessBindingFs.cpp", "ProcessBindingFs.lut.h"],
  ["src/jsc/bindings/ProcessBindingNatives.cpp", "ProcessBindingNatives.lut.h"],
  ["src/jsc/bindings/ProcessBindingHTTPParser.cpp", "ProcessBindingHTTPParser.lut.h"],
  ["src/jsc/modules/NodeModuleModule.cpp", "NodeModuleModule.lut.h"],
  ["src/jsc/bindings/webcore/JSEvent.cpp", "JSEvent.lut.h"],
  [dir + "/codegen/ZigGeneratedClasses.lut.txt", "ZigGeneratedClasses.lut.h"],
];
for (const [src, out] of lutPairs) {
  await $`env TARGET_PLATFORM=linux bun ${dir}/bun/src/codegen/create-hash-table.ts ${src} ${dir}/codegen/${out}`.cwd(dir + "/bun");
}
console.log("wrote codegen/{json,xml}_byte_class.{h,rs} + build_options.rs + jsc-tier codegen + C++ headers/luts");
