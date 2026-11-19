#!/usr/bin/env node
// SPDX-License-Identifier: MIT
// Builds the page into dist/:
//   node build.mjs --relay <origin> [--dev] [--out dist]
// --relay is the relay's origin (https://host[:port], http only for loopback);
// it is compiled in and becomes the CSP's connect-src (serve.mjs reads it from
// dist/.build.json). --dev adds the "Copy pairing text" button (with a warning),
// which a local run needs to hand the pairing text to vault-dev.
//
// Needs vault-web built first (in vault-web/js: npm ci, npm run build, npm run build:wasm).
// The wasm is bundled into main.js as bytes, since the CSP has no connect-src 'self'.

import { copyFileSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import * as esbuild from "esbuild";

import { checkRelayOrigin, cspFor } from "./serve.mjs";

const here = dirname(fileURLToPath(import.meta.url));

export function parseArgs(argv) {
  const opts = { relay: undefined, dev: false, out: join(here, "dist") };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--dev") opts.dev = true;
    else if (a === "--relay" || a === "--out") {
      const v = argv[++i];
      if (v === undefined) throw new Error(`${a} needs a value`);
      if (a === "--relay") opts.relay = v;
      else opts.out = resolve(v);
    } else throw new Error(`unknown argument ${a}`);
  }
  if (opts.relay === undefined) throw new Error("--relay <origin> is required");
  opts.relay = checkRelayOrigin(opts.relay);
  return opts;
}

export async function build({ relay, dev, out }) {
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
    define: { __RELAY_ORIGIN__: JSON.stringify(relay), __DEV__: String(dev) },
    // Folds `if (__DEV__)` away in release builds.
    minifySyntax: true,
    legalComments: "none",
    logLevel: "warning",
  });
  // The same CSP as a <meta> too, for hosts that do not send the header (a
  // meta policy cannot enforce frame-ancestors: send the header where you can).
  const html = readFileSync(join(here, "index.html"), "utf8");
  if (html.split("__CSP__").length !== 2) throw new Error("index.html must have exactly one __CSP__ placeholder");
  writeFileSync(join(out, "index.html"), html.replace("__CSP__", cspFor(relay)));
  copyFileSync(join(here, "styles.css"), join(out, "styles.css"));
  writeFileSync(join(out, ".build.json"), `${JSON.stringify({ relayOrigin: relay, dev }, null, 2)}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const opts = parseArgs(process.argv.slice(2));
    await build(opts);
    console.error(`built ${opts.out} (relay ${opts.relay}${opts.dev ? ", DEVELOPMENT build with the copy button" : ""})`);
  } catch (e) {
    console.error(`build.mjs: ${e.message}`);
    process.exit(1);
  }
}
