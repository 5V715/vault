// SPDX-License-Identifier: MIT
// A tiny browser consumer of the package, as an app depends on it: a temp project
// with node_modules/vault-web linked to this package, importing only the public paths
// ("vault-web", "vault-web/web", "vault-web/web/vault_web_bg.wasm"). It must
// type-check with tsc and bundle with esbuild (platform browser); an unresolved
// import fails. Needs `npm run build` and `npm run build:wasm` first.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import * as esbuild from "esbuild";

const pkgDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const MAIN = `import * as glue from "vault-web/web";
import wasmUrl from "vault-web/web/vault_web_bg.wasm";
import { loadWeb, pair, type Session } from "vault-web";

export async function start(): Promise<Session> {
  await loadWeb(glue, new URL(wasmUrl, location.href));
  return pair({
    relayOrigin: "https://relay.example",
    appId: "vault-todo",
    deviceName: "bundle check",
    onOffer: (o) => console.log(o.qr?.size),
    onSas: (s) => console.log(s),
    onState: (s, reason) => console.log(s, reason),
  });
}
`;

const TSCONFIG = {
  compilerOptions: {
    target: "ES2022",
    module: "ESNext",
    moduleResolution: "Bundler",
    // The wasm-bindgen d.ts declares [Symbol.dispose].
    lib: ["ES2022", "DOM", "ESNext.Disposable"],
    strict: true,
    noEmit: true,
    types: [],
  },
  files: ["main.ts", "wasm.d.ts"],
};

function main() {
  for (const f of ["dist/index.js", "pkg/web/vault_web.js", "pkg/web/vault_web_bg.wasm"]) {
    if (!existsSync(join(pkgDir, f))) {
      throw new Error(`${f} is missing: run \`npm run build\` and \`npm run build:wasm\` first`);
    }
  }
  const dir = mkdtempSync(join(tmpdir(), "vault-web-consumer-"));
  try {
    mkdirSync(join(dir, "node_modules"));
    symlinkSync(pkgDir, join(dir, "node_modules", "vault-web"), "dir");
    writeFileSync(join(dir, "main.ts"), MAIN);
    writeFileSync(join(dir, "wasm.d.ts"), 'declare module "*.wasm" {\n  const url: string;\n  export default url;\n}\n');
    writeFileSync(join(dir, "tsconfig.json"), JSON.stringify(TSCONFIG, null, 2));

    const tsc = spawnSync(process.execPath, [join(pkgDir, "node_modules/typescript/bin/tsc"), "-p", dir], {
      encoding: "utf8",
    });
    if (tsc.status !== 0) throw new Error(`the consumer does not type-check:\n${tsc.stdout}${tsc.stderr}`);
    console.log("tsc: the consumer type-checks");

    const out = join(dir, "out");
    const r = esbuild.buildSync({
      absWorkingDir: dir,
      entryPoints: ["main.ts"],
      bundle: true,
      platform: "browser",
      format: "esm",
      outdir: out,
      loader: { ".wasm": "file" },
      metafile: true,
      logLevel: "warning",
    });
    if (r.errors.length) throw new Error(`esbuild: ${r.errors.length} errors`);
    const inputs = Object.keys(r.metafile.inputs);
    for (const want of ["pkg/web/vault_web.js", "dist/index.js"]) {
      if (!inputs.some((i) => i.endsWith(want))) {
        throw new Error(`the bundle does not include ${want}; inputs: ${inputs.join(", ")}`);
      }
    }
    const files = readdirSync(out);
    if (!files.some((f) => f.endsWith(".wasm"))) throw new Error(`no .wasm asset emitted: ${files}`);
    console.log(`esbuild: bundled ${inputs.length} inputs into ${files.join(", ")}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

try {
  main();
} catch (e) {
  console.error(`bundle-check: ${e.message}`);
  process.exit(1);
}
