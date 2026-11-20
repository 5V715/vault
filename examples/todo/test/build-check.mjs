// SPDX-License-Identifier: MIT
// Builds the page (release and --dev) into temp dirs and checks the output:
// the files, the relay compiled in, the wasm embedded (no .wasm fetch), the copy
// button only in the dev build, and no inline script or style in index.html.
// Needs vault-web built (dist and pkg/web).

import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { build, parseArgs } from "../build.mjs";
import { cspFor, metaCspFor } from "../serve.mjs";

const pkgWeb = resolve(dirname(fileURLToPath(import.meta.url)), "../node_modules/vault-web/pkg/web/vault_web_bg.wasm");

async function main() {
  if (!existsSync(pkgWeb)) {
    throw new Error("vault-web's pkg/web is missing: in vault-web/js run npm ci, npm run build, npm run build:wasm");
  }
  assert.throws(() => parseArgs(["--relay", "https://relay.example/path"]), /relay origin/);
  assert.throws(() => parseArgs(["--relay", "http://relay.example"]), /relay origin/);
  assert.throws(() => parseArgs([]), /--relay/);

  const base = mkdtempSync(join(tmpdir(), "todo-build-"));
  try {
    const rel = join(base, "release");
    const dev = join(base, "dev");
    await build(parseArgs(["--relay", "https://relay.example", "--out", rel]));
    await build(parseArgs(["--relay", "http://127.0.0.1:8787", "--dev", "--out", dev]));

    const files = readdirSync(rel).sort();
    assert.deepEqual(files, [".build.json", "index.html", "main.js", "styles.css"], "no separate .wasm");
    assert.deepEqual(JSON.parse(readFileSync(join(rel, ".build.json"), "utf8")), {
      relayOrigin: "https://relay.example",
      dev: false,
    });
    const relJs = readFileSync(join(rel, "main.js"), "utf8");
    const devJs = readFileSync(join(dev, "main.js"), "utf8");
    assert.ok(relJs.includes('"https://relay.example"'), "the relay origin is compiled in");
    assert.ok(devJs.includes('"http://127.0.0.1:8787"'));
    assert.ok(!relJs.includes("clipboard"), "no clipboard access in a release build");
    assert.ok(devJs.includes("clipboard"), "the dev build copies the pairing text");
    for (const devOnly of ["Copy pairing text", "Development build", "dev-copy"]) {
      assert.ok(!relJs.includes(devOnly), `release bundle contains dev-only ${JSON.stringify(devOnly)}`);
      assert.ok(devJs.includes(devOnly), `dev bundle lacks ${JSON.stringify(devOnly)}`);
    }
    assert.ok(!/__DEV__|__RELAY_ORIGIN__|__RUNTIME_CONFIG__/.test(relJs), "defines replaced");
    assert.ok(!/\binnerHTML\b|\bouterHTML\b|insertAdjacentHTML|document\.write/.test(relJs), "no HTML sinks in the bundle");

    const html = readFileSync(join(rel, "index.html"), "utf8");
    const meta = html.match(/<meta http-equiv="Content-Security-Policy" content="([^"]*)">/);
    assert.ok(meta, "a CSP meta tag");
    assert.equal(meta[1], metaCspFor("https://relay.example"), "the meta CSP is the header's");
    assert.equal(
      `${meta[1]}; frame-ancestors 'none'`,
      cspFor("https://relay.example"),
      "the meta CSP is the served header minus frame-ancestors",
    );
    assert.ok(meta[1].includes("connect-src wss://relay.example;"));
    assert.ok(!html.includes("__CSP__"));
    const devHtml = readFileSync(join(dev, "index.html"), "utf8");
    assert.ok(devHtml.includes("connect-src ws://127.0.0.1:8787;"));
    assert.ok(!/<script(?![^>]*\bsrc=)[^>]*>/i.test(html), "no inline script");
    assert.ok(!/<style\b|\sstyle=|\son[a-z]+=/i.test(html), "no inline style or handlers");

    // --runtime-config (the image): no relay compiled in, /config.json read at
    // start, no <meta> CSP (vault-relay sends the header).
    const rt = join(base, "runtime");
    await build(parseArgs(["--runtime-config", "--out", rt]));
    assert.deepEqual(readdirSync(rt).sort(), [".build.json", "index.html", "main.js", "styles.css"]);
    assert.deepEqual(JSON.parse(readFileSync(join(rt, ".build.json"), "utf8")), {
      relayOrigin: null,
      runtimeConfig: true,
      dev: false,
    });
    const rtJs = readFileSync(join(rt, "main.js"), "utf8");
    assert.ok(!/__DEV__|__RELAY_ORIGIN__|__RUNTIME_CONFIG__/.test(rtJs), "defines replaced");
    assert.ok(rtJs.includes('"/config.json"'), "reads /config.json");
    assert.ok(!relJs.includes("/config.json"), "baked-in builds do not fetch /config.json");
    assert.ok(!/relay\.example|127\.0\.0\.1:8787/.test(rtJs), "no relay compiled in");
    assert.ok(!rtJs.includes("clipboard"), "release: no clipboard");
    const rtHtml = readFileSync(join(rt, "index.html"), "utf8");
    assert.ok(!/Content-Security-Policy|__CSP__/.test(rtHtml), "no meta CSP in a runtime-config build");
    assert.ok(rtHtml.includes('<script type="module" src="main.js"></script>'));
    console.log(`build-check: ok (release main.js ${relJs.length} bytes, dev ${devJs.length} bytes)`);
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
}

try {
  await main();
} catch (e) {
  console.error(`build-check: ${e.stack ?? e}`);
  process.exit(1);
}
