// SPDX-License-Identifier: MIT
/**
 * The page's entry: load the vault-web wasm, then hand the screens to `TodoApp`.
 * build.mjs defines `__RELAY_ORIGIN__` (from `--relay`), `__RUNTIME_CONFIG__`
 * (`--runtime-config`: the relay is read from /config.json instead, see
 * config.ts) and `__DEV__` (`--dev`). The wasm is bundled as bytes, so loading it
 * needs no fetch (the CSP's connect-src lists the relay, plus 'self' for
 * /config.json in runtime-config builds only).
 */

import { loadWeb, pair } from "vault-web";
import * as glue from "vault-web/web";
import wasmBytes from "vault-web/web/vault_web_bg.wasm";

import { TodoApp } from "./app.js";
import { loadRuntimeRelay } from "./config.js";
import { createUi } from "./view.js";

async function main(): Promise<void> {
  const root = document.getElementById("app");
  if (!root) throw new Error("vault-todo: #app missing");
  const ui = createUi(document, root, { dev: __DEV__ });
  try {
    await loadWeb(glue, wasmBytes);
  } catch (e) {
    console.error("vault-todo: loading vault-web failed:", e);
    ui.showClosed("Could not load the vault module.", { onPairAgain: () => location.reload() });
    return;
  }
  let relayOrigin = __RELAY_ORIGIN__;
  if (__RUNTIME_CONFIG__) {
    try {
      relayOrigin = await loadRuntimeRelay((url, init) => fetch(url, init), location.protocol);
    } catch (e) {
      console.error("vault-todo:", e);
      ui.showClosed("Could not load the relay configuration.", { onPairAgain: () => location.reload() });
      return;
    }
  }
  const app = new TodoApp({
    pair: (opts) => pair(opts),
    ui,
    relayOrigin,
    deviceName: "Vault to-do (browser)",
    dev: __DEV__,
    // Development builds only: vault-dev takes the pairing text on its command line.
    ...(__DEV__ ? { copyText: (text: string) => navigator.clipboard.writeText(text) } : {}),
  });
  app.start();
}

void main();
