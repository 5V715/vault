// SPDX-License-Identifier: MIT
// Builds the wasm package: `cargo build -p vault-web --target wasm32-unknown-unknown
// --release`, then `wasm-bindgen` into pkg/node (`--target nodejs`, for Node and the
// e2e tests) and pkg/web (`--target web`, for browsers).
//
// wasm-bindgen-cli must be exactly the version of the `wasm-bindgen` crate in
// Cargo.lock (the generated glue and the wasm must match); install it with
//   cargo install --locked wasm-bindgen-cli --version <that version>

import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "../..");
const TARGET = "wasm32-unknown-unknown";

function run(cmd, args, opts = {}) {
  console.error(`+ ${cmd} ${args.join(" ")}`);
  const r = spawnSync(cmd, args, { cwd: repoRoot, stdio: "inherit", ...opts });
  if (r.error || r.status !== 0) {
    throw new Error(`${cmd} failed (status ${r.status}${r.error ? `, ${r.error}` : ""})`);
  }
  return r;
}

/** The `wasm-bindgen` crate version pinned in Cargo.lock. */
export function lockedWasmBindgenVersion(lock) {
  const m = lock.match(/\[\[package\]\]\nname = "wasm-bindgen"\nversion = "([^"]+)"/);
  if (!m) throw new Error("wasm-bindgen not found in Cargo.lock");
  return m[1];
}

function main() {
  const want = lockedWasmBindgenVersion(readFileSync(join(repoRoot, "Cargo.lock"), "utf8"));
  const v = spawnSync("wasm-bindgen", ["--version"], { encoding: "utf8" });
  const have = v.status === 0 ? v.stdout.trim().split(/\s+/)[1] : undefined;
  if (have !== want) {
    throw new Error(
      `wasm-bindgen-cli ${want} is required (Cargo.lock), found ${have ?? "none"}: ` +
        `cargo install --locked wasm-bindgen-cli --version ${want}`,
    );
  }

  run("cargo", ["build", "-p", "vault-web", "--release", "--locked", "--target", TARGET]);
  const meta = spawnSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], {
    cwd: repoRoot,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (meta.status !== 0) throw new Error(`cargo metadata failed: ${meta.stderr}`);
  const wasm = join(JSON.parse(meta.stdout).target_directory, TARGET, "release", "vault_web.wasm");

  for (const [target, dir, type] of [
    ["nodejs", "node", "commonjs"],
    ["web", "web", "module"],
  ]) {
    const out = join(here, "pkg", dir);
    rmSync(out, { recursive: true, force: true });
    mkdirSync(out, { recursive: true });
    run("wasm-bindgen", ["--target", target, "--out-dir", out, wasm]);
    // This package is "type": "module"; the nodejs glue is CommonJS.
    writeFileSync(join(out, "package.json"), `${JSON.stringify({ type }, null, 2)}\n`);
  }
  console.error(`built ${join(here, "pkg")} (wasm-bindgen ${want})`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  try {
    main();
  } catch (e) {
    console.error(`build.mjs: ${e.message}`);
    process.exit(1);
  }
}
