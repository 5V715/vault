// SPDX-License-Identifier: MIT
/**
 * Node 22+ entry: loads the `--target nodejs` wasm module from `pkg/node`
 * (built by `npm run build:wasm`) and re-exports the API. The mailbox uses
 * Node's global `WebSocket`. Outside a browser `pair` needs `testOrigin`.
 */
import { setWasm, type WasmBindings } from "./index.js";

export * from "./index.js";

const url = new URL("../pkg/node/vault_web.js", import.meta.url);
let mod: { default?: WasmBindings } & Partial<WasmBindings>;
try {
  mod = await import(url.href);
} catch (e) {
  throw new Error(
    `vault-web: cannot load the wasm package at ${url.pathname}; build it with \`npm run build:wasm\` (${String(e)})`,
  );
}
// A CommonJS module's exports are its named exports, and also its default.
setWasm(typeof mod.pair === "function" ? (mod as WasmBindings) : (mod.default as WasmBindings));
