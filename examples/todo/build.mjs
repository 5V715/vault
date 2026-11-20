#!/usr/bin/env node
// SPDX-License-Identifier: MIT
// Builds the page into dist/:
//   node build.mjs --relay <origin> [--page-origin <origin>] [--dev] [--out dist]
//   node build.mjs --runtime-config [--dev] [--out dist]
// --relay is the relay's origin (https://host[:port], or its wss:// form; http or
// ws only for loopback); it is compiled in and becomes the CSP's connect-src
// (serve.mjs reads it from dist/.build.json). --page-origin is where the page
// will be served: when given, the relay must use the same security (an https
// page with a ws: relay is mixed content the browser blocks; a wss: relay for an
// http page is refused as a mistaken setup). --dev adds the "Copy pairing text" button (with a warning),
// which a local run needs to hand the pairing text to vault-dev.
// --runtime-config (the vault-todo-demo image) compiles no relay in: the page
// reads it from /config.json, which `vault-relay --static-dir` writes from its
// PUBLIC_ORIGIN/RELAY_ORIGIN and allows with the CSP it sends (connect-src 'self'
// plus the relay). Such a build has no <meta> CSP (it cannot know the relay): the
// header from vault-relay is the policy. See docs/deploy-todo-demo.md.
//
// Needs vault-web built first (in vault-web/js: npm ci, npm run build, npm run build:wasm).
// The wasm is bundled into main.js as bytes, since the CSP has no connect-src 'self'.

import { copyFileSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import * as esbuild from "esbuild";

import { checkOrigin, checkRelayOrigin, metaCspFor } from "./serve.mjs";

const here = dirname(fileURLToPath(import.meta.url));

export function parseArgs(argv) {
  const opts = { relay: undefined, pageOrigin: undefined, runtimeConfig: false, dev: false, out: join(here, "dist") };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--dev") opts.dev = true;
    else if (a === "--runtime-config") opts.runtimeConfig = true;
    else if (a === "--relay" || a === "--out" || a === "--page-origin") {
      const v = argv[++i];
      if (v === undefined) throw new Error(`${a} needs a value`);
      if (a === "--relay") opts.relay = v;
      else if (a === "--page-origin") opts.pageOrigin = v;
      else opts.out = resolve(v);
    } else throw new Error(`unknown argument ${a}`);
  }
  if (opts.runtimeConfig) {
    if (opts.relay !== undefined) throw new Error("give either --relay or --runtime-config, not both");
    if (opts.pageOrigin !== undefined) {
      throw new Error("--page-origin does not apply to --runtime-config (vault-relay checks PUBLIC_ORIGIN)");
    }
    return opts;
  }
  if (opts.relay === undefined) throw new Error("--relay <origin> or --runtime-config is required");
  opts.relay = checkRelayOrigin(opts.relay);
  if (opts.pageOrigin !== undefined) {
    opts.pageOrigin = checkOrigin(opts.pageOrigin, "page origin");
    const pageTls = opts.pageOrigin.startsWith("https:");
    const relayTls = opts.relay.startsWith("https:");
    if (pageTls && !relayTls) {
      throw new Error(
        `the https page ${opts.pageOrigin} cannot use the ws: relay ${opts.relay} (mixed content, ` +
          "the browser blocks it): give the relay as wss://host:port",
      );
    }
    if (!pageTls && relayTls) {
      throw new Error(
        `a wss: relay (${opts.relay}) for the http page ${opts.pageOrigin} is refused: serve the page ` +
          "over https too, or use a loopback ws: relay for local development",
      );
    }
  }
  return opts;
}

/** index.html without the <meta> CSP (and the comment above it): runtime-config builds. */
function withoutMetaCsp(html) {
  const out = html.replace(/\n[ \t]*<!--[^\n]*-->\n([ \t]*<meta http-equiv="Content-Security-Policy" content="__CSP__">)/, "\n$1");
  const lines = out.split("\n").filter((l) => !l.includes('content="__CSP__"'));
  if (lines.length !== out.split("\n").length - 1) throw new Error("index.html: the CSP <meta> line not found");
  return lines.join("\n");
}

export async function build({ relay, runtimeConfig = false, dev, out }) {
  rmSync(out, { recursive: true, force: true });
  mkdirSync(out, { recursive: true });
  await esbuild.build({
    absWorkingDir: here,
    entryPoints: ["src/main.ts"],
    outfile: join(out, "main.js"),
    bundle: true,
    format: "esm",
    platform: "browser",
    target: "es2022",
    loader: { ".wasm": "binary" },
    define: {
      __RELAY_ORIGIN__: JSON.stringify(runtimeConfig ? "" : relay),
      __RUNTIME_CONFIG__: String(runtimeConfig),
      __DEV__: String(dev),
    },
    // Folds `if (__DEV__)` away in release builds.
    minifySyntax: true,
    legalComments: "none",
    logLevel: "warning",
  });
  // The same CSP as a <meta> too, for hosts that do not send the header, minus
  // frame-ancestors (a meta policy cannot carry it): send the header where you can.
  const html = readFileSync(join(here, "index.html"), "utf8");
  if (html.split("__CSP__").length !== 2) throw new Error("index.html must have exactly one __CSP__ placeholder");
  writeFileSync(join(out, "index.html"), runtimeConfig ? withoutMetaCsp(html) : html.replace("__CSP__", metaCspFor(relay)));
  copyFileSync(join(here, "styles.css"), join(out, "styles.css"));
  const info = runtimeConfig ? { relayOrigin: null, runtimeConfig: true, dev } : { relayOrigin: relay, dev };
  writeFileSync(join(out, ".build.json"), `${JSON.stringify(info, null, 2)}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const opts = parseArgs(process.argv.slice(2));
    await build(opts);
    const relay = opts.runtimeConfig ? "from /config.json at run time" : opts.relay;
    console.error(`built ${opts.out} (relay ${relay}${opts.dev ? ", DEVELOPMENT build with the copy button" : ""})`);
  } catch (e) {
    console.error(`build.mjs: ${e.message}`);
    process.exit(1);
  }
}
